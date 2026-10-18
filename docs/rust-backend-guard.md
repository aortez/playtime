# Backend ownership foundation — #60 / #23 O2a

`playtime-backend-guard` supplies a CPU-testable host ownership service/library.
Its only executable is **`playtime-backend-guard-fixture`**, explicitly selected
by the test harness. It cannot connect to Ollama, SSH, systemd, Docker, the public
API or experimental archives. No managed host mode is installed by this slice.

The subsequent [host boundary](rust-backend-host.md) supplies staged O2b ports
and policy; its dedicated-host activation and live gate remain separate.

```sh
./scripts/test-rust-backend-guard.sh
```

The gate starts actual guard and independent fixture-manager subprocesses in
private temporary directories. The manager owns two actual children and retains
their `Child` handles across guard death. A stop must kill **and reap both**;
socket close, a recorded PID, and a manager's diagnostic reply do not suffice.
This demonstrates local IPC and ownership behavior, not production enforcement.
All its tests also run in the ordinary locked workspace suite. No GPU, Docker,
systemd, network service, live model or historical run data is required.

## Compiled contract

`playtime_protocol::backend_guard` owns an independent version-1 private wire
contract. Typed JSON objects reject unknown/duplicate fields; identifier, path,
length, deadline and checked-arithmetic validation precedes mutation/allocation.
The protocol has no token-usage settlement, archive receipt or frontier type.

| Record | Immutable binding |
| --- | --- |
| `BackendIdentity` | Opaque backend and guard-state IDs, boot, unit, original invocation/cgroup, monotonically increasing generation, exact profile/lock fingerprint |
| `LeaseSpec` | Acquisition operation and lease IDs; service/run/manifest, original worker/grant, owner revision, fixed host execution and cleanup deadlines |
| `LeaseRecord` | Lease specification, SHA-256 verifier of a random private capability, last ordinal and retirement tombstone |
| `RequestSpec` | Lease/generation, ordinal, request/effect IDs, exact body SHA-256 and length, original maximum allowance, fixed request deadline |
| `RequestRecord` | Original dispatch decision time and `may_have_dispatched` or verified protocol terminal |
| `StopOperation` | First operation ID/reason/decision, original backend binding, fixed supervisor and observation deadlines |
| `JournalRecord` | Version, consecutive sequence, previous record's exact SHA-256 and one typed transition |

Capabilities are 256 random bits, redacted from Rust diagnostics. The caller
chooses a capability before acquisition so an identical lost-reply retry needs
no plaintext secret in the journal. Private control authentication is separate
from per-lease dispatch authorization. Neither secret goes into command lines,
normal diagnostics or experimental inputs; fixture bootstrap files are 0600.

`Guard::{acquire,dispatch,revoke,release,status}` implement the service boundary.
An identical acquisition retry waits for the original durable operation even
while publication is pending. Conflicting operation reuse fails. Competition
receives Busy without queuing another lease. Dispatch ordinal is exactly the
previous ordinal plus one; duplicate request/effect identities, changed caps or
bytes and retired capabilities cannot start another effect. A matching request
retry within the reserved lease returns that request's status only, including
after an ambiguous restart. It never calls the backend again.

`Status.sequence` identifies the latest selected state; `durable_sequence` is
the last acknowledged journal publication. They can differ while I/O is pending
or uncertain. A control result never claims durability merely because selected
state is visible. Acquire/release and successful revoke acknowledgments wait
for publication. Revoke can return Storage while independent stop continues.
`termination_recorded` is historical durable evidence; `Gate::Preparing` after
restart means a fresh held termination capability is still being acquired.

## Trusted ports and transition ordering

`Backend::verify_read_only` is restricted to bounded, read-only exact identity
verification. There is no model-management operation. The returned
`VerifiedBackend` holds a non-serializable `BackendAuthority`. The future O1
adapter join supplies real profile verification; the fixture uses a synthetic
profile. Preparing a replacement generation is a trusted provisioning method,
not a private client command or a restart fallback.

Before `Backend::execute` is called, the journal must durably publish the exact
`DispatchMarked` record. Even a crash before the first byte is transmitted keeps
that record conservative. The trusted port sends once, checks cancellation before
transmission and returns `VerifiedTerminal` only after its protocol validates a
terminal bound to the exact backend/request. Partial observations remain opaque
bytes. A protocol terminal is durably committed before the final stream control
reply. It neither settles the experiment nor proves backend process exit.

Normal terminal completion permits the same live lease's next unique request.
The future runner must enforce its separate committed archive-settlement barrier.
Normal release needs no pending request and retires the capability durably before
another lease. Model residency alone does not require a restart. A worker that
misses a terminal reply can still have an unresolved **full original allowance**
in its archive even when the host knows the protocol finished.

Owner transport loss during a request, deadline, explicit revoke or uncertain
backend failure closes the dispatch gate before I/O and chooses one stop. The
owner here is the executing worker's private inference connection, not a UI or
public API observer. A completed older stream cannot revoke a later request.

The journal runs on a dedicated thread. Stop selection, timer servicing and the
supervisor task never wait behind response forwarding or private-file I/O. A
stop publication gets a 100 ms acknowledgment window; uncertainty permanently
blocks this guard's admission while the supervisor still receives the original
binding. Detached operations survive caller cancellation. At most 16 mutation
waiters are retained, separately from payload/control socket quotas.

`Supervisor::stop` must freshly authenticate the original manager identity,
submit/recover the same operation and enforce its supplied deadline without
reading worker diagnostics. `ManagerObservation` is only diagnostic. Only
`Supervisor::terminated` can return a non-serializable `HeldTermination` backed
by a retained `TerminationAuthority`. Release and replacement preparation require
that held capability, matching the exact original instance and operation.
The fixture's authenticated manager bridge retains an owned connection to the
manager that reaped the children and has no replacement-spawn operation. A
serialized `terminated` observation does not implement this port contract.

Stop retries on manager unavailability keep the same operation and deadlines.
An invocation mismatch blocks reuse and never targets the replacement. Confirmed
termination is persisted before release. Lost acknowledgments can adopt the same
confirmed result after fresh authentication; they do not repeat inference.

## Persistence, restart and bounds

The root is a service-provisioned **backend-host** private directory, separate
from any checkout, experiment or daemon database. A stable `.lock` inode remains
held for the guard's lifetime, including after journal failure. Two processes
cannot authorize through the same root. Production must enforce the single
configured host authority and prevent direct inference bypass; a caller choosing
an arbitrary second directory is not a host access-control mechanism.

Records are consecutive immutable `00000001.json` files. Each file and directory
are synced; a separately published `head.json` binds the last committed sequence
and hash, detecting missing committed tail files. A complete unacknowledged
record is validated and re-synced on reopen. Partial, missing, conflicting,
unsupported or corrupt state fails closed. There is no repair, truncation,
pruning, empty-ledger fallback or replay-tombstone erasure. Explicit creation
requires an empty private root; ordinary reopen never creates a ledger.

Restart first takes exclusion and validates the entire journal. **Every surviving
lease enters stop reconciliation**, including one whose last request has a
terminal. This conservative rule covers a gate closure whose stop write was
lost. Terminal history remains queryable; the request is never resent. A saved
stop retains its first decision/reason/deadlines. Without a saved stop, restart
uses the earlier durable request-mark time (or the earlier of now and the lease
execution deadline), and a deterministic lease-bound stop ID. A process restart
or lock reacquisition never increments the backend generation. After stop and
release, trusted preparation can increment generation by exactly one, with a new
invocation and fresh verified profile. Old tombstones remain. Different boots
and unexpected replacement instances block automatic reuse.

| Resource | Fixed maximum |
| --- | --- |
| Control JSON or journal record | 64 KiB |
| Request body | 4 MiB, validated against exact length and digest |
| Partial response payload | 512 KiB/frame, 4 MiB total |
| Forwarding queue | 1 MiB shared across streams, including frames being written; 64 slots/stream |
| Private state | 64 MiB and 10,000 entries, including lock/head and staging |
| Admission reserve | Eight 64 KiB records retained for stop/termination/release; head/staging capacity is also reserved |
| Connections | 16 control and 2 inference connections; separate listeners/quotas |
| Private control/setup | 2 seconds server-side; 3 seconds client-side |
| Read-only verification or supervisor exchange | At most 1 second, stop observation shortened by remaining host deadline |

Unix IPC uses same-UID peer checks plus a private service credential, version and
nonce. Control is length-prefixed big-endian u32 JSON. Dispatch uses the separate
inference socket: bounded control header, exactly the declared body bytes, then
an initial control reply. Subsequent frames have tag 1 + u32 length + opaque
partial bytes, or tag 2 + a final bounded control reply. These fixed five-byte
framing headers are separate from the payload bound. EOF is not a terminal.
Duplicate dispatch returns only the status reply and closes; it has no stream.
Library port implementations must be asynchronous and cancellation-cooperative.

The clock supplies boot-bound absolute milliseconds (`CLOCK_BOOTTIME` in the
local implementation). Execution and cleanup deadlines never renew on retry.
The supervisor bound is **90 seconds** from the first stop decision; the stop
observation bound is **120 seconds**, both shortened by the lease's fixed cleanup
deadline. Expiry blocks admission and retains uncertainty, never manufactures
exit. Revocation may still submit the original expired stop; it cannot extend
grace. Cross-host clock mapping and crash-independent host deadline enforcement
remain O2b/O2c responsibilities.

## Executable evidence and remaining integration

The gate covers two-service acquisition contention, same-key preparation retries,
body/cap conflicts, old ordinals, revoked capabilities after generation rollover,
normal release, and a completed stream detached during the next request. Explicit
publication barriers kill the guard before/after dispatch marking, write/sync,
upstream submission and terminal publication. The independent upstream counts
zero or one send, never a replay. Other process cases cover socket loss with
surviving children, a remaining descendant, lost stop replies, unavailable and
returning managers, a replacement process with the same unit, stalled/failed
private I/O, cancelled callers, full response streams, malformed control frames,
damaged/missing state, capabilities absent from the journal and staging/entry
budgets. Virtual clocks cover the fixed policy bounds without 90/120-second sleeps.

O2b must bind these ports to authenticated SSH and real manager/cgroup authority,
enforce deadlines across guard/SSH loss, install reviewed Rust-only access control
and provide explicit activation/rollback with a dedicated-host fault gate. O2c
must join the unchanged #55 parser/transport, freeze worker inputs/capabilities,
map host deadlines conservatively, preserve raw observations and archive settlement
barriers, and join #59/#43 recovery ownership. The real Docker/Ollama two-session
run-and-view and production cancellation/restart/worker-loss gates remain open.
This foundation does not close #23, #20, #21 or #22.

Validation for this slice: formatting and locked all-target workspace Clippy;
387 passing workspace Rust tests (14 explicitly ignored opt-in/helper cases);
23 passing guard/protocol gate tests, included in that workspace total; Python
96 tests with six Docker opt-in skips. The guard gate completes in about seven
seconds after compilation on the development host. No live inference was run.
