# Prompt comparison in three chunks

This exploratory comparison for [issue #15](https://github.com/aortez/playtime/issues/15)
tests three invitations using the existing Python controller. It develops the
[persona and discovery ideas](persona-and-discovery.md) while the Rust foundation
is built. Each chunk contains all three conditions for one sampling seed, runs
sequentially, and ends with a pause before the next chunk.

See the [first chunk's results](prompt-screening-results.md) for the current
pause point, local evidence, and commands for continuing.

| Condition | Prompt | Recipe |
| --- | --- | --- |
| Current | Existing curiosity invitation | [curiosity.toml](curiosity.toml) |
| Minimal | Exactly "welcome, have fun" plus a final newline | [prompt-minimal.toml](prompt-minimal.toml) |
| Persona | Current invitation plus an invented background and evolving interests | [prompt-persona.toml](prompt-persona.toml) |

The [minimal prompt](../prompts/minimal-invitation.txt) retains the standard
protocol, inventory, tool descriptions, and automatic controller messages.
It tests a short invitation; discovering the memory mechanism remains a later
experiment. The [persona prompt](../prompts/developing-persona.txt) invites the
agent to choose its own backstory and develop it through experience. It provides
no example character and preserves factual checking of workshop claims.

## Frozen conditions and order

| Chunk | Sampling seed | Order |
| --- | ---: | --- |
| 1 | 1042 | Current, Minimal, Persona |
| 2 | 2042 | Minimal, Persona, Current |
| 3 | 3042 | Persona, Current, Minimal |

All nine runs start fresh. Earlier runs are background evidence, not controls
for this comparison. Each uses curiosity's model and image locks, seed files,
protocol, sampling settings, and limits. Memory is explicitly `self`, without a
factual appendix. Only prompt, sampling seed, and identifying bookkeeping differ.

Each run permits ten sessions with ten work turns per session, 100,000 charged
generation tokens, and 900 seconds of agent work. The ceilings are 300,000 tokens
and 45 minutes per chunk, or 900,000 tokens and 135 minutes for all nine runs.
Snapshotting and cleanup add time. Unknown usage remains a separate conservative
reservation; charged tokens are not all measured generation.

Freeze all nine input archives before the first inference. Use the same archived
Python controller for every run, preserving the locked model and workshop.
Existing controller APIs can prepare and execute these archives; the issue #1
six-run pilot command has a different fixed protocol and is not used here.

The local study folder under `runs/studies/` records the immutable plan, manifest
hashes, controller identity, and a separate progress ledger. Its study-specific
launcher accepts an explicit chunk number and launches only that chunk. It
validates frozen inputs and runtime identities, holds the existing project run
lock, and skips every previously started slot. Rerunning a chunk command may
execute its remaining unstarted slots; it never retries an attempted run.
An interruption ends the invocation after the current run's cleanup. The exact
study path and commands are recorded with the local results.

## Review and stopping

Review each completed chunk before deciding when to start the next. Keep prompts
and budgets frozen across chunks, and give the model no additional coaching.
Retain failures, early finishes, and interrupted runs. If later discussion calls
for changed conditions, create a separate study instead of altering these slots.

For each run, record the initial project choice and subsequent changes in
direction, interests carried across sessions, corrections or repeated mistakes,
and whether claimed persona motives connect to concrete actions. An evolving
autobiography alone does not establish an effect on behavior. Check substantive
artifact claims independently inside the workshop and preserve uncertainty.

Report known generation, unknown reservations, charged tokens, calls, elapsed
time, session count, stop reason, and links to the saved reports. Keep fiction,
intentions, and factual workshop claims distinct. Retain representative evidence
from events and checkpoints so observations can be reviewed.

Three seeds support exploration, not a reliable effect estimate. The persona
condition also adds context and may consume tokens on narration. Runtime
nondeterminism, early stops, unequal realized work, pauses between chunks, and
human judgment limit causal claims. Conclusions from the first chunk are
provisional observations from one seed.
