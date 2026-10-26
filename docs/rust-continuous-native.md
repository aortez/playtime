# Continuous compaction: native execution

#110 step B connects the shared `continuous_compaction_v1` controller to the
packaged worker and versioned archive. This is an explicit native preparation
capability. Catalog recipes, preview/launch APIs and workbench selection remain
step C; no existing recipe changes its harness or allocations.

## Preparation and ownership

`ContinuousProfile::freeze` produces `docker_continuous_compaction_v1`. Its
validator reconstructs the exact supported template, including seeds, tool
responses, limits, helper/image identities and model settings. The worker bundle's
native capability digest includes that template and the reviewed model lock.
`VerifiedBundle::prepare_continuous` checks capability compatibility before
freezing the archive; worker admission checks the profile family, manifest v2,
executable, mode, allocations and backend identity again. The test-only workspace
probe is still not a supported production worker profile.

This fixed smoke profile allocates 300 seconds, 16,384 generated tokens, a
16,384-token context, 512 tokens per model call, 45 seconds per inference,
2,048 retained tool-output bytes and 16 MiB of log evidence. It uses the policy's
75% trigger, two retained exchanges and a 512-token summary cap. Short smoke
instructions request a summary of at most 300 characters; this is task text,
not an additional truncation rule. General policy defaults remain unchanged.
Live work and summary requests use the same reviewed Qwen3:1.7b identity and
settings, with reasoning off and the context allocation frozen explicitly.

The worker acquires one volume, holder and run-scoped agent through the existing
private ownership journal and exact identity checks. `start_workspace` carries
no session identity and cannot restart after quiescence. Compaction performs only
model inference: it does not stop, capture or replace the workshop. The absolute
work deadline never renews. Finalization stops the agent, captures the workspace,
publishes through the owned writer and cleans up. Intermediate workspace captures
are unavailable. A completion declaration is separate from capture/publication
and cleanup success; lost receipts never authorize a final capture.

## Archive and read contract

The input manifest and execution records use the existing v2 family. The archive
retains settings, exact model requests/responses, native completion metadata,
before/after conversations, summary content, estimates and their bases, and
purpose-specific accounting. Ollama's first terminal `done_reason` is forwarded
without inference. Only `stop` can establish normal summary completion; missing,
partial and `length` outcomes cannot replace active history.

`compaction_planned`, `compaction_applied` and `completion_declared` observations
include `evidence.schema_version = 1` links. Each source names a request ID,
purpose and optional observation location (controller sequence, archive sequence
and exact record hash). Plans link their older and retained work requests plus
the preceding summary request. Replacements link the summary request and retained
plan; completion declarations link their work request. The full request/response
stream remains available through those request identities. Locations identify
bytes; they are not publication receipts.

Storage checks the links against admitted run-scoped requests and retained
observations, including externalized payloads. Missing source/plan locations
remain null rather than pointing to unrelated evidence. `compaction_history`
pages over a bounded archive window and returns recorded or explicitly omitted
entries. Its cursor advances over all records, so a page with no compaction rows
can still have a next page. Callers must follow `has_more` and
`next_after_sequence`, not infer completion from an empty result.

## Native gates

Use the locked Docker environment, user systemd and a private short temporary
path. Run one live check at a time, after confirming no Playtime worker is active:

```sh
TMPDIR=/home/data2/pt ./scripts/test-rust-continuous.sh --backend docker-scripted
TMPDIR=/home/data2/pt PLAYTIME_LIVE_ENDPOINT=http://127.0.0.1:11435 \
  ./scripts/test-rust-continuous.sh --backend live
```

The live endpoint must be an IP literal and expose the exact reviewed local Qwen
identity. See [local Ollama setup](local-ollama-development.md). This additional
gate does not replace the original GPT-OSS service run-and-view gate.

Both modes use the static bundle and real systemd supervisor, authenticated Begin,
Docker workshop, controller and archive writer. The seeded probe launches a
background counter and advances 48 stages, checking the same live process each
time. Its final result requires that counter to have advanced. The gate requires
at least two accepted compactions, a sole final capture containing the result,
reconciled summary usage, no unknown usage, and exact absence of all three owned
resources after cleanup. It compares the final offline read frontier with the
worker's authenticated receipt; the offline reader itself grants no authority.

Every attempt retains its printed evidence directory, frozen inputs, archive,
accounting and compaction projections, worker status and cleanup observations.
The first live development attempt used an ambiguous task and repeatedly supplied
a filename as `cwd`; it completed with zero compactions and correctly failed the
gate. A second attempt used valid arguments but declared completion after only
three probe stages, also failing the gate. The task now specifies one call per
response, exact arguments, and explicit incomplete-stage feedback. Neither failed
attempt is treated as continuity evidence.

Development evidence on 2026-10-26 (UTC), retained under `/home/data2/pt/`:

| Attempt | Result |
| --- | --- |
| `pc.gpQZCO0w` | Live: incorrect working directory, zero compactions; rejected by the gate |
| `pc.VaDmlptc` | Live: premature completion after three stages; rejected by the gate |
| `pc.ikHca3pe` | Live: seven compactions, then incomplete archive without a terminal publication; exact cleanup verified. Cause unconfirmed; retained as failed evidence. Worker diagnostics now report execution/publication failures to the journal. |
| `pc.8vwoWKHH` | Live: seven compactions, 51 work calls, 1,882 generated tokens (105 for summaries), zero unknown usage, final capture and all three owned resources absent |
| `pc.ugoZHQMB` | Scripted Docker: 15 compactions, 49 work calls, 2,048 generated tokens, final capture and all three owned resources absent |

CPU regressions cover malformed source links, missing/truncated completion
metadata, failed summary and cancellation finalization, lost capture receipts,
changed native profiles and uncertain workspace starts. The existing controller
golden covers policy decisions. Native success does not establish restart,
reattachment, replay or archive continuation.
