# Memory evidence pilot (issue #1)

Condition A is `memory_condition = "self"` (the default). Condition B is
`memory_condition = "self_plus_evidence"`. Both use the unchanged inspiration,
protocol, tools, workspace, sampling settings and limits in `curiosity.toml`.
The appendix is a separate user message at the next session's start; session 1
receives none. It uses the ordinary context guard and receives no extra budget.

## Appendix selection, version 1

The controller derives observations from checkpoint indexes and explicitly
dispatched `exec` records, without inference. The first checkpoint is compared
with the frozen initial workspace, subsequent checkpoints with their predecessor.
Paths are workspace-relative. No host paths or raw logs are supplied to the model.

The complete UTF-8 text, including its instructions and count footer, is at most
4,096 bytes. File changes are ordered added/modified/removed, then by path; their
JSON lines have a 900-byte allocation and names have 160-byte prefixes. Commands
with missing exit codes, nonzero exits, timeouts or errors come first, newest
dispatch first; remaining commands follow newest first. Command prefixes have
240 bytes, working directories 160 bytes, and stdout/stderr/error prefixes
200 bytes each. Whole JSON lines
are selected while leaving 512 bytes for the footer; non-fitting lines are
omitted and selection continues. UTF-8 characters are never split. Counts record
omitted files/commands, shortened fields and excerpt bytes omitted. Upstream
truncation is retained separately; its missing byte count is unknown.

Only dispatches count, never rejected tool requests or parser failures. A dispatch
does not prove that the shell started; missing results remain unknown. Zero exit
status describes process completion, not artifact correctness. Command/output
text is explicitly quoted data, not instructions. The original bounded tool
result is used even when context fitting shortened what the prior session saw.

Each checkpoint archives `evidence.json` and `evidence.txt`; the next session
archives `evidence-received.json` and `evidence-received.txt`. These hold the exact
supplied text, byte count, SHA-256, checkpoint paths/hashes, and source event
sequence numbers/hashes. `MEMENTO.md` is unchanged. Repeat validation requires the
recorded session directories, reconstructs the appendix and checks its received
copy against the recorded session start. Sessions interrupted before a checkpoint
remain valid without a generated appendix.
Older archives without this feature remain condition A.

## Frozen paired workflow

First run the controller suite, Docker integration suite and scripted smoke.
Commit the experimental controller before freezing. Then:

```sh
./playtime pilot-prepare experiments/curiosity.toml
python3 runs/<first-run-id>/inputs/controller/playtime --root "$PWD" \
  pilot-run runs/pilots/<pilot-id>
```

Preparation freezes all six archives before inference and prints their IDs. The
pilot plan records pair membership, order, manifest hashes and shared identities.
Execution checks all archives, their recipes and pair membership against the plan
before launch, rejects duplicate run IDs, and rechecks model/image identity for
each run. A project lock serializes the entire batch. Order is 1042 A/B, 2042 B/A,
3042 A/B. Only condition, seed and bookkeeping differ. `progress.json` retains
every started slot, even failures; invoking the same plan again skips them.
An infrastructure exception stops the batch; resuming retains that failed slot.
Use the archived controller command above to isolate execution from later edits.

The issue's curiosity limits bound the batch to six runs, 600,000 charged generated
tokens and 90 minutes of agent work, plus setup/cleanup. Fixed seeds do not ensure
identical GPU execution. Do not coach, retry failures or replace unsuccessful runs.

## Review

For every checkpoint, quote each concrete checkable memento claim and classify it
as supported, contradicted or unverified against that checkpoint's files and tool
records. Keep intentions/preferences separate. Track next-session repetition,
correction or action. Count contradicted and unverified claims separately,
repeated failed commands and sessions changing only memory. Independently test
artifacts inside the isolated workshop, preserving commands/results and snapshot
identities. File changes and exit 0 alone are not useful-progress measures.

Report per-run usage (known and unknown reservations), elapsed time, calls, stop
reason, handoffs, received appendix bytes, claim audit and artifact checks. Show
all three paired comparisons with local report links. This pilot cannot estimate
a reliable effect size or separate factual content from added-context effects.

The completed six-run pilot and local audit links are in
[memory-evidence-results.md](memory-evidence-results.md).
