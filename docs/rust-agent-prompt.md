# Playtime agent prompt

New chat conversations use `--agent-prompt playtime`, a prompt written for
Playtime: in the workshop console, with `--preset coding-host`, and for one-shot
`--prompt` turns and launcher `--launch-request` tasks. It is a
[shared core](../prompts/playtime-coding-v1.txt) followed by a tools section for
[coding-host](../prompts/playtime-host-v1.txt) or
[the workshop](../prompts/playtime-workshop-v1.txt), and the client appends the
environment inventory. Everything the prompt says should be true of Playtime's
actual runtime. Headless workers and read-only roles keep their own fixed
instructions.

## Why it replaced the Codex composition

The previous default, `codex`, was OpenAI's Codex CLI 0.166.0 system prompt,
copied unedited from a Codex session for
[a prompt experiment](../experiments/codex-default-prompt.md), followed by a
Playtime adapter that overrode it where the two conflicted. That composition:

- Opened with a false identity: "You are a coding agent running in the Codex
  CLI, a terminal-based coding assistant. Codex CLI is an open source project
  led by OpenAI." The adapter corrected it about 21,000 characters later. On
  2026-11-26, asked for its system prompt, Gyro-M said that it "sets me up as a
  coding agent in a terminal-based environment (Codex CLI running inside
  Playtime)".
- Described tools Playtime doesn't have (`apply_patch`, `update_plan`, approval
  modes) and AGENTS.md injection that Playtime doesn't do. The adapter had to
  countermand each one.
- Gave formatting rules for Codex's renderer, while Playtime's terminal prints
  replies as plain text.
- Took 26,814 characters for coding-host and 26,261 for the workshop, before the
  inventory. The Playtime prompt takes 8,146 and 7,324.

The experiment kept the Codex text unedited to test whether that exact prompt
helps. It began from two GPT-OSS 120B runs in the Codex CLI itself, where the
prompt describes the real tools: the run with the Codex prompt used fewer tool
calls and tokens than one with a custom prompt, which the write-up calls
"promising observations, not a controlled effect estimate". Its only completed
pilot in Playtime, one task with one seed, passed 7 of 10 checks with the Codex
prompt and 9 of 10 with a basic one, and the write-up concludes that it "does
not reproduce a general improvement". No measured result justifies keeping text
that misdescribes the runtime.

## What changed

Everything is in Playtime's own wording; "kept" means the instruction survives.

| Codex composition | Playtime prompt | Why |
| --- | --- | --- |
| Codex identity and capability list | Replaced by a statement of what Playtime is | False here |
| Personality, preambles and progress updates | Kept, condensed | Useful habits; preambles go in the same response as the tool calls |
| Planning with `update_plan`, with example plans | Dropped; state a short plan in the reply when it helps | The tool doesn't exist |
| AGENTS.md rules | Kept, corrected | Playtime doesn't inject AGENTS.md, so the model must read the files |
| Keep going until done; don't guess | Kept | |
| Permission to work on proprietary code, analyze vulnerabilities and show code | Kept | |
| Coding guidelines | Kept, except "no inline comments" and "no one-letter names" | Projects set those in their own conventions |
| `apply_patch` usage | Edit with `write_file` or `exec` | The tool doesn't exist |
| Validation approach | Kept | |
| Holding off on checks under approval modes | Dropped; run checks without asking | Playtime has no approval modes |
| Ambition versus precision | Kept, one line | |
| Final-answer formatting and file-reference syntax for Codex's renderer | Replaced by plain-terminal guidance | Markdown isn't rendered |
| Warning about inline citation markup | Dropped | An OpenAI citation format |
| Prefer `rg`; don't print files through scripts | Kept; `rg` only when installed | |
| Adapter: working with the user, terminal controls, compaction, completion | Kept, condensed | Already Playtime's own text |
| Adapter: tools and workspace | Became the coding-host and workshop sections | |

## Selecting and reopening

The startup banner names the prompt, for example `Agent prompt: playtime`. A
conversation keeps the prompt it was created with. Under another prompt, the
session picker lists it as `[settings differ]`, and `--session` refuses it with
"session configuration differs from the selected model/workspace/policy". Reopen
it with its own prompt: `--agent-prompt codex` for the previous default, or
`codex-v1` or `minimal`. The Qwen3-1.7B and LFM2.5-350M llama-server presets
still default their workshop console to `minimal`; Qwen3.5-0.8B uses `playtime`.
See the
[launcher guide](rust-chat-launcher.md#local-llama-server-profiles).

## Changing the prompt

Saved conversations bind their exact prompt text, and a `prompt.rs` test pins the
SHA-256 of every released composition. Never edit a released prompt file. Add a
new version, such as `playtime-coding-v2.txt`, point `playtime` at it, and keep
the old composition selectable under a versioned name such as `playtime-v1`, as
`codex-v1` is.

Compare a change by sending the same requests with the old and new prompts.
Chat sessions use a fixed sampling seed, and on 2026-11-26 an identical request
reproduced Gyro-M's earlier reply exactly. This is one observed replay, not a
guarantee of deterministic inference. Keep model settings and workspace inputs
fixed, and compare multiple paired seeds. Measure coding work with the
[coding-repair assessments](../assessments/coding-repair-v1/README.md).
