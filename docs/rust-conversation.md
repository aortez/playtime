# Shared compaction context planner

`playtime_engine::conversation::ContextPolicy` is the reusable context-planning
component for the compacting harness. The existing `continuous_compaction_v1`
policy uses it, and the standalone conversation test exercises the same planner
without an experiment, archive, workshop or service.

This is the first extraction for [#208](https://github.com/aortez/playtime/issues/208).
The interactive session loop, persistence, workspace tools, Strata transport and
[chat console](https://github.com/aortez/playtime/issues/210) still need their
implementation. The existing controller remains the execution runtime.

## Caller contract

Create a `ContextPolicy` with validated `CommonLimits`, `CompactionSettings` and
the exact tool definitions used by the model. Configuration is frozen inside
the policy. `initial_fits` checks that the original instructions and task leave
room for work and its reserves.

Implement `History` over the caller's transcript. Its exchanges must be complete
assistant responses with all their tool results. `work_messages` includes the
original instructions/task, previous summary, retained failure facts, complete
exchanges and pending input. `summary_messages(older)` includes only the older
complete exchanges with the task, previous summary and summary instructions.
Pending tools must finish or be reconciled before presenting a history view.

Pass the runtime's remaining time/tokens and measured model pace as `Budget`.
Maintain `ContextWindow` with the exact dispatched messages/tools and their
observed usage. Its calibrated estimates remain heuristics, with the same
limitations described in [the controller contract](rust-controller.md#continuous-compaction-policy).
Unknown usage must not be replaced with zero or a made-up server measurement.

`plan` returns work messages, a summary plan, or a stop reason. A summary plan
identifies the number of older exchanges and retains its before/after sizing
basis. Optional deferral observations explain when the preference trigger is
passed because a summary is unlikely to finish in the remaining time. Hard
context boundaries never defer for that reason.

After a normally completed, nonempty, tool-free summary, construct the candidate
replacement while preserving original instructions, recent complete exchanges
and recorded failure facts. Call `replacement` with the plan's `before_bytes`
before discarding history. It checks that the replacement shrinks and leaves
room below the trigger with the required reserves. The caller is responsible
for the content and integrity of that replacement; a fit check is not evidence
that the model retained every fact.

The planner grants no execution authority and does not settle usage, validate
model/tool output, retry effects, enforce deadlines, publish artifacts or save
sessions. Those responsibilities remain with the runtime and its adapters.
Existing experiments retain all controller admission and cleanup checks.

## Verification

The standalone test supplies a caller-owned chat history, compacts complete tool
exchanges, preserves a pending follow-up and resumes planning. It also rejects
a summary that leaves insufficient generation budget for continued work.
The existing controller compaction suite checks the integrated decisions,
failure history, retries, accounting and unchanged record golden.

```sh
cargo test -p playtime-engine --features test-harness --locked
cargo test -p playtime-runner --features test-harness --test workspace_probe --locked
```

These checks are CPU-only and execute no model-generated command or live model
request. They do not establish a working chat console or Strata integration.
