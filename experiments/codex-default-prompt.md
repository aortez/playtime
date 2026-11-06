# Codex default instructions in Playtime: prompt comparison

Status: the 35B BF16 pilot is complete; the owner rejected 27B BF16 throughput
after its first attempt. Neither 35B output met every task requirement. No
existing recipe limits changed. The follow-up
model-selection change defaults new selections to their
reviewed context window. This is a new Rust experiment, independent of the frozen
Python prompt study.

## Question and evidence

Does adding the exact Codex base prompt from the owner's successful local-model
test improve tool use, verified results, and token efficiency in Playtime?

The November 3, 2026 notes session “Explain framework device usage” reviewed two
GPT-OSS 120B Codex runs on the same GitLab question. Directly re-reading their
logs gives:

| Provider-reported metric | Custom base | Default base |
| --- | ---: | ---: |
| Base instruction characters | 1,427 | 20,751 |
| Shell tool calls | 61 | 34 |
| Generated output tokens | 11,583 | 7,209 |
| Final request input tokens | 41,562 | 22,395 |
| Cumulative input plus output tokens | 1,403,270 | 426,984 |

The earlier review found that the default run successfully queried approvals,
whereas the custom run inferred no approvals from failed or incomplete queries.
This preparation verified the counts, not a fresh audit of every factual claim.
Both runs retained additional developer guidance. Live GitLab data changed
between them, and both first requests reported 2,448 input tokens despite
different base lengths. These are promising observations, not a controlled
effect estimate or independently verified tokenizer counts. Cumulative input
counts repeat the growing conversation; they are not peak context occupancy.

## Frozen instruction candidates

[Source metadata](codex-default-prompt-v1.json) records the exact session,
Codex version, SHA-256 digests, and current recipe identity. The
[base prompt](../prompts/codex-cli-0.166.0-base.txt) is copied verbatim from that
session's `session_meta.base_instructions.text`, including its original final
newline behavior. It is not a reconstruction from the older binary extraction
in the notes repository, nor the instructions of the agent preparing this study.

The proposed system-message compositions are:

- A: existing Playtime protocol, then the shared compatibility adapter.
- B: frozen Codex base, existing Playtime protocol, then the same adapter.

Join components with two newline characters. The
[adapter](../prompts/codex-playtime-adapter-v1.txt) maps execution to Playtime's
actual tools and lifecycle. Both arms receive identical adapter bytes, so only
the Codex base differs. This tests adding the full base under Playtime's
constraints, not recreating the entire Codex runtime. A later distilled-prompt
condition must be separately named rather than called the default prompt.

Keep the task invitation, seed files, inventory, tool schemas, model digest,
inference target, reasoning and sampling settings, harness, compaction settings,
workspace, and budgets identical within each pair. The owner selected
Qwen3.8-27B BF16 and Qwen3.6-35B-A3B BF16 for the first pilot, using a fixed
offline coding task. Compare A and B within each model; this does not directly
replicate the GPT-OSS 120B notes result. The checked-in curiosity recipe still
selects the original 20B model.

## Integration constraints before launching

For memento mode, `recipe.protocol` becomes the system message. Ordinary recipe
prompt editing changes user task text, not the system protocol. For continuous
mode, the frozen `compaction.protocol` supplies the system instructions; changing
the source recipe's memento protocol does not change that field. Continuous
protocol construction currently uses versioned built-in files. Do not silently
modify those defaults or the separate compaction summarizer for this experiment.

The current curiosity recipe's 65,536-token context does not admit the full
Codex addition under the conservative, uncalibrated startup guard. Even without
the adapter, its calculated requirement is 68,374; the original requires 46,935.
Those values include output/handoff reserves and count canonical input bytes as
tokens. They are admission estimates, not measured model token counts. The
metadata records the estimates with the shared adapter as well.

The owner subsequently requested model-dependent capacity. New model selections
now default to the reviewed window, up to Playtime's 131,072-token maximum, with
an editable override. At 98,304 or 131,072, the memento treatment's 70,410 startup
estimate fits without reducing its existing reserves or tool-output allowance.
This does not measure actual token usage or host memory fit.

Before execution, validate the selected model/host and run Rust admission on the
frozen pilot recipes. Use the same effective model context in both arms and
preserve their other settings. Any later continuous-mode experiment needs its
own admission check, not the memento calculation above.

## Evaluation

First use a fixed offline task with independently checkable outputs to measure
completion and efficiency. Then use the same curiosity invitation and seed pack
for a separate behavioral comparison. Open-ended work has no fixed amount of
output: fewer tokens alone can mean less work, and more tool calls can mean more
useful work. Do not mix the two task types in one score.

Run fresh workspaces sequentially, retain every attempt, and use paired sampling
seeds with reversed A/B order across pairs. One pair is a pilot. Before starting,
freeze all inputs and define the artifact checks; review the pilot before more
runs. Check for other active Playtime workers and coordinate other inference
clients on the chosen host.

Record valid tool calls, malformed/invented calls, repeated failures, recovery,
tool-output bytes, verified completed requirements, unsupported claims, elapsed
time, stop reason, and cleanup. Separate known generated tokens, reported input
tokens, peak context, summary/handoff usage, and unknown-usage reservations. Do
not report unknown usage as zero or cumulative input as context occupancy.

## BF16 pilot preparation, 2026-11-03

The fixed task is [ledger repair](../seeds/bf16-ledger-v1/README.md), with a
deliberately flawed starter and sample CSV. The model must deliver a working
standard-library CLI, exact JSON report, and notes about its actual checks.
The independent evaluator tests exact arithmetic, row validation, duplicate
handling, sorting, empty input, header errors, missing input, and deliverables.
It runs candidate code only in the locked workshop image as uid 1000, with no
network, read-only candidate files and bounded CPU, memory, processes and time.
Before inference, its reference implementation passed 10/10 checks and the
flawed starter passed 2/10. README accuracy also requires manual transcript
review; file existence alone does not prove the claims in it.

Prepared inputs and retained setup attempts are at
`/home/data2/playtime/qwen-bf16-20261104`. `pilot-plan.json` hashes both recipes,
all seed files and the evaluator before inference. Each run has 98,304 context
tokens, a 600-second wall budget, seed 1042 and explicit thinking On. Other
allocations retain the curiosity recipe's reviewed values. The planned order was
27B A/B, then 35B B/A, with fresh workspaces. The actual sequence was 27B A,
then 35B B/A after the owner rejected 27B throughput. Only the 35B pair completed;
there is no repeatability estimate. No Q8 arm is part of this pilot.

The original 0.32.3 runtime refused the Qwen3.8 pull as requiring a newer
version. A separately installed Ollama 0.40.14 runtime uses its own model store
and loopback listener; its release archives passed their published SHA-256
checks. Existing runtime binaries and model locks are preserved. Both models
passed native 96K probes with default thinking, On and Off, including tool calls,
tool-result receipts, streaming and cancellation. The
[hardware record](../docs/rust-framework-models.md#bf16-qwen-candidates-2026-11-03)
separates these compatibility checks from the coding pilot.

The 27B basic-prompt attempt (`20261104T051256Z-8d2db31b`) ended with
`model_timeout` after 455.968 seconds. It completed two inspection commands, then
hit the existing 400-second deadline on its third model call. Cleanup completed;
the downloaded checkpoint contains only the unchanged seeds. The archive reports
131 known generated tokens and one unknown-usage request with an 8,192-token
reservation; that reservation is not measured generation. The owner considered
the observed throughput too slow, so its Codex-prompt arm is not being run. This
single stopped arm establishes no prompt comparison for 27B. The 35B B/A pair
used the original 600-second wall and 400-second per-call limits;
the proposed longer limits were not selected.

## Completed 35B BF16 pilot

Basic arm A is `20261104T052831Z-65a2eee3`; Codex arm B is
`20261104T052123Z-8497fb01`. Both used the same locked model, task, seed files,
98,304 context, sampling settings and bounds. B ran first. Both reached
`agent_finished` after ten work calls and one forced handoff, with complete
cleanup and no unknown-usage requests. This stop reason records the agent's
decision; it does not certify task correctness.

| Observed metric | Basic A | Codex B |
| --- | ---: | ---: |
| Frozen acceptance checks passed | 9/10 | 7/10 |
| Elapsed time | 428.477 s | 295.046 s |
| Known generated tokens, including handoff | 7,418 | 4,242 |
| Handoff generated tokens | 860 | 579 |
| Cumulative reported input tokens | 65,818 | 102,315 |
| Reported input plus generated tokens | 73,236 | 106,557 |
| Largest completed-request input | 9,366 | 10,952 |
| Tool calls, including one `end_session` | 13 | 14 |
| Tool/model protocol errors | 0 | 0 |
| Serialized tool-result payload bytes | 7,898 | 6,316 |

Input counts repeat context across requests; they are neither peak occupancy nor
uncached prefill work. Tool-result sizes use compact, sorted-key UTF-8 JSON of
each recorded result payload. A used seven exec, three read_file and two
write_file calls; B used eight exec, four read_file and one write_file call,
plus one end_session each. Neither needed recovery from a malformed tool call.
Both inspected the specification and starter before writing, ran the sample and
additional checks, then claimed all requirements were met.

A correctly retained customers with zero settled totals and supplied all three
deliverables. Its frozen failure accepted full-width Unicode digits despite the
ASCII-only amount requirement. B incorrectly removed customers with zero totals,
failing two cases, and omitted the required README. Its final memory both claimed
all deliverables were complete and identified the README as still missing.

The frozen suite was not exhaustive. Subsequent code review prompted two
separate checks, retained in `posthoc-qwen36-{a,b}.json`: A converted `-2` to
positive 200 cents, whereas B correctly returned -200. Both accepted a header
with leading whitespace despite the exact-header requirement. These checks were
added after seeing the outputs and are not folded into the frozen 10-check score.
A passed 0/2 and B 1/2 of these additional checks. Neither output is fully correct.

Manual transcript review confirms A ran the listed groups of checks, but its
README overstates coverage: a zero-valued transaction belonging to a customer
with a nonzero total does not test retaining a zero-total customer. It also says
the output file must not already exist, although the program opens it for
overwriting. Both agents' blanket completion claims exceed the evidence.

In this pair, B used 42.8% fewer generated tokens and finished 31.1% sooner,
but used one more tool call and 45.5% more reported input-plus-output tokens.
The pilot therefore does not reproduce a general improvement in tool use,
correctness and total token use. One task, one seed and a single B/A order cannot
establish repeatability or separate order/cache effects. No curiosity run or Q8
comparison was performed.

`results.json`, per-arm reports, full events, verified workspace downloads,
frozen and post-hoc evaluations are retained under the evidence root above.
Candidate programs were never repaired by the evaluator or executed on the host.
The temporary daemon, SSH tunnel and isolated Ollama service were stopped after
all runs completed. Framework's original Ollama 0.32.3 service was restored and
checked idle; downloaded weights and the separate 0.40.14 installation remain.
Rust formatting, workspace Clippy and tests, the frozen Python suite, scripted
Docker run-and-view, and the original-model live run-and-view checks passed.
These checks do not establish production restart/recovery behavior.

## Reusing the coding tests

New runs should use the versioned [coding-repair benchmarks](../assessments/coding-repair-v1/README.md):
ledger, dependency planner and byte ranges. Their independent Docker assessment
works with captured Codex or Rust Playtime workspaces and keeps task completion
separate from check results. Supply only each bundle's seed files to the model.

The `bf16-ledger-v1` files here remain unchanged historical inputs for this
pilot. They use Playtime-specific paths and completion instructions; the later
`ledger-repair-v1` bundle has its own frozen task and checker identities. Do not
silently substitute it when replaying this pilot or overwrite the original
scores with a newer assessment. No second assessment runner is needed here.

## Later behavioral study

For curiosity, also record whether the agent inspects seeds before choosing a
project, persists after errors, checks artifacts, changes direction, writes
useful memory, and spends more effort planning or narrating. Expect that the
coding-oriented default might change project choices as well as execution
quality. Treat all such effects as hypotheses until observed in retained runs.
