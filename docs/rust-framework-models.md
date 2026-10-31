# Framework large-model exploration

This is an explicit extension of the registered-model workflow, not a change to
the original GPT-OSS 20B lock or recipe. The candidates are
`qwen3-coder-next:q4_K_M` and `gpt-oss:120b` on the Framework Radeon 8060S host,
using Ollama 0.32.3. Model files and experimental archives stay outside Git.
Exact identity locks and reviewed capability profiles belong in the repository.

## Candidate review

The metadata-only `review_model` example takes an exact expected manifest digest
and writes a new evidence directory. It uses the bounded native discovery reader
and canonical identity encoding. It does not update an existing lock or grant
production execution approval.

```sh
cargo run --locked -p playtime-ollama --example review_model -- \
  http://framework.local:11434 MODEL EXPECTED_DIGEST default /absolute/new-evidence
```

Adding `--live` sends three bounded requests: a synthetic `record_number(7)` tool
call, a follow-up verifying a unique tool-result receipt, and a streaming request
cancelled after its first native record. It allocates a 65,536-token context and
retains exact requests and observed responses. No model command is executed.
Supported probe reasoning arguments are `default`, `off`, `on`, `low`, `medium`
and `high`; a probe argument is a candidate for validation, not a capability claim.
The default request allowance is 512 tokens. `--live MAX_TOKENS` explicitly
selects 1–8,192 tokens while retaining the 180-second request deadline. Keep a
failed smaller-budget probe when testing a larger allowance; this does not
change any controller recipe limits.

First check that no Playtime worker or other inference client is active. Run one
probe at a time. After cancellation, independently inspect the model host for
idle inference before another request: the adapter's cancelled result is not
held backend termination authority. Inspect GPU residency, allocation, swap and
server/kernel logs. Download size alone does not establish GPU memory fit.

## Explicit selection

After the exact identity and supported settings have been reviewed and compiled,
rebuild the worker bundle and daemon together. Their capability digest includes
the new locks. Registration preserves existing source recipe provenance.

The `select_model` client example registers the inspected identity and saves an
immutable clone with a complete model settings JSON. It retains each exact keyed
request before submission, never retries a mutation, and never launches inference.

```sh
cargo run --locked -p playtime-client --example select_model -- \
  CONNECTION SOURCE_RECIPE MODEL SETTINGS_JSON NEW_EVIDENCE_DIR EXPECTED_DIGEST [TARGET]
```

A daemon started with [named targets](rust-inference-targets.md) needs the
`TARGET` ID: the review and the final preview run on that machine. It prints the
saved execution recipe ID. Launch that ID through the ordinary Rust CLI with a
retained idempotency key and the same `--target`. A failed mutation keeps its
pending receipt for lookup; do not rerun blindly with a new key. The examples do
not add HTTP routes, ledger versions or CLI commands.

Coder-Next has non-thinking execution; model-default omission and explicit Off
are separate wire choices. High/Medium/Low and On are not converted to Off.
GPT-OSS 120B retains model-default omission and Low/Medium/High. Its boolean
thinking settings remain unsupported. Context ceilings describe the locked model,
not validated GPU allocation: the common recipe bounds still cap context at
131,072 tokens.

## Exploration design

The initial comparison uses the original curiosity prompt, protocol, seeds and
workshop with a 600-second wall budget. It retains ten sessions and all other run
limits. Coder-Next uses temperature 1.0, top-p 0.95, top-k 40, repeat penalty 1.0,
seed 1042, 65,536 context and explicit Off. GPT-OSS 120B uses the original high
reasoning and temperature 0.8 with the same remaining settings. The separate
short two-session probe uses the reviewed live-smoke allocations.

Assess checked artifacts, useful progress, tool errors, handoffs and cleanup.
Across model families, tokenizers and reasoning behavior differ, so token rates
do not alone measure useful work. Unknown usage remains unknown; interrupted or
malformed responses are not assigned zero tokens. Every attempted run, including
failures, is retained.

## Evidence

Evidence is retained outside Git at `/home/data2/playtime/large-1027`.
Coder-Next passed the three-request native probe with both omitted thinking and
explicit Off. Both inspections produced the same canonical definition digest
`60419550c62d7490ec3f12c46c97c9635bd3b13d7457e7cb4ebd0dc457e950a5`.
Ollama reported 49/49 layers on the GPU, a 49,171 MiB ROCm model buffer and an
816 MiB KV buffer at 65,536 context. Server logs independently recorded task
cancellation and all slots idle.

GPT-OSS 120B passed with omitted thinking, Low and Medium using the 512-token
probe. Its first High request exhausted 512 tokens in thinking without a tool
call. A separately retained High probe with a 2,048-token allowance passed;
its tool call used 227 tokens and its receipt response used 77. This repeat does
not establish deterministic behavior or isolate the token cap as the sole cause
of the first failure. All inspections agreed on definition digest
`acfeec8b10460604fd54148f537fc42a909d0947459c33347ebacb617be006f6`.
Ollama reported 37/37 layers on the GPU, a 61,224 MiB ROCm model buffer and
1,267 MiB of KV buffers at 65,536 context. Each successful cancellation probe
was followed by independently observed server cancellation and idle slots.

Both models ran with the existing host configuration. The host showed 5.7 MiB
of swap use after loading; this is not evidence of zero swapping over the whole
trial. These are short native compatibility probes.

Both models passed the separate two-session Playtime persistence smoke, with
`agent_finished`, two verified checkpoints, the expected file read in session 2,
no tool errors and complete cleanup. Coder-Next used 170 known tokens and
GPT-OSS 120B used 211. The latter smoke explicitly used Low reasoning.

Coder-Next's curiosity run (`20261027T223206Z-95c3e642`) reached the 600-second
wall limit with complete cleanup, four sessions and 44 completed responses. It
generated 15,789 known tokens at 43.07 tokens/second of recorded generation time;
the final cancelled request has unknown usage, with its 8,192-token reservation
retained. There were 11 tool/program errors: four program exits, four unknown
tool names, two tools unavailable during handoff and one missing input path.
The main visualization script finally ran successfully around 454 seconds.

Its final checkpoint contains three generated PNGs and three new Python scripts
alongside the seeds and memento. Download hashes, PNG CRC/decompression and Python
syntax were checked without executing model code on the host. The combined
signal/Rule-110 image is legible; the signal-only plot has poor label contrast.
The DFT script ran successfully in the workshop, reconstructing its input to
2.81e-14, but its arbitrary magnitude threshold omitted the weaker spectral
component. The last memento describes session 3 and omits the session-4 DFT work.

GPT-OSS 120B's curiosity run (`20261027T224332Z-bb92b1ad`) also reached the
600-second wall limit with complete cleanup. It started three sessions (the
third only in the final seconds), completed 23 responses and generated 16,834
known tokens at 33.58 tokens/second. One malformed tool call and the final
cancelled request have unknown usage, retaining 16,384 reserved tokens. Its
21 tool calls included six errors: three invalid working directories, two
attempts to open Python source as an image, and one unavailable handoff tool.
The first handoff was missing. Session 2 built a second renderer rather than
reusing the first session's work, then saved a useful memento near the deadline.

The final checkpoint contains four valid PNGs (including 2000×2000 output and
an escape-iteration histogram), three new Python scripts and an area estimate
of 1.51274475. The estimate uses a finite 2000×2000 grid and 500 iterations;
it is not an independently established true area. The final memento records
these limitations, but its file description incorrectly says the analysis
script creates the high-resolution image; a separate generator invocation did
that. The high-resolution render has weak interior/exterior contrast from its
cyclic colormap. Artifact hashes, PNG decoding and Python syntax were checked,
and the high-resolution image was visually inspected.

Both archives' byte counts, SHA-256 frontiers, manifest identities and complete
API event streams were cross-checked. Backend logs independently show task
cancellation and idle slots at each deadline. Neither run flagged a model
reload or queue wait after its initial load. Both runs' end-of-run Framework
kernel-log queries were empty; its swap reading remained 5.7 MiB.

### Comparison with the earlier sanity runs

| Model and host | Known generation tokens/s | Completed responses | Tool/program errors |
| --- | ---: | ---: | ---: |
| GPT-OSS 20B, RTX 4090 | 182.11 | 105 | 17 |
| GPT-OSS 20B, Framework | 46.60 | 36 | 11 |
| Coder-Next Q4_K_M, Framework | 43.07 | 44 | 11 |
| GPT-OSS 120B, Framework | 33.58 | 23 | 6 |

Rates sum known generated tokens over matching Ollama generation durations;
they include reasoning tokens and exclude requests with unknown usage. Error
counts are not rates: Coder-Next made 57 tool calls, while GPT-OSS 120B made 21.
All runs share the curiosity prompt, protocol, seeds, workshop and numeric
budgets. Coder-Next explicitly changes reasoning to Off and temperature to 1.0;
GPT-OSS 120B retains High and 0.8. The 4090 run ended at its ten-session limit
before ten minutes. The new runs use a rebuilt, frozen worker bundle containing
the reviewed profiles; source provenance includes the uncommitted snapshot.

These single autonomous trajectories establish hardware fit and usable native
integration, not a model-quality ranking. Coder-Next had more effective
handoffs but spent much of its budget debugging; GPT-OSS 120B repeated work
after losing a handoff and later produced a more explicit quantitative result.
A useful next comparison would use a fixed task and repeated trials, including
GPT-OSS 120B at Medium reasoning, before changing the provider architecture.

### Validation and retained evidence

Formatting, workspace Clippy, the full Rust test suite, and the frozen Python
suite passed (101 Python tests, six skipped). The packaged-worker gate and real
Docker-scripted/live run-and-view gates passed; the live regression used 20B on
the 4090. These gates do not establish the deferred restart/recovery behavior.
The later probe-budget example change was additionally compiled, linted and
exercised against GPT-OSS 120B.

`/home/data2/playtime/large-1027/results.json` contains the verified metrics and
baseline reference. `evidence/` retains probes, failed attempts, input equality
checks, smoke verification, host logs, test logs and copied native-gate archives.
`artifacts/{coder,gpt120}/index.json` maps original workspace names to verified
downloads and receipts. The models remain installed on Framework; temporary
experiment daemons are stopped after collection.

## References

- [Coder-Next model card](https://huggingface.co/Qwen/Qwen3-Coder-Next)
- [Ollama Coder-Next Q4_K_M](https://ollama.com/library/qwen3-coder-next:q4_K_M)
- [GPT-OSS 120B model documentation](https://developers.openai.com/api/docs/models/gpt-oss-120b)
- [Ollama GPT-OSS 120B](https://ollama.com/library/gpt-oss:120b)
- [Registered recipe selection](rust-recipes.md#registered-model-selection)
- [Live-model execution and exclusion](rust-live-model.md)
