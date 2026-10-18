# Controller observation mapping v1

These are invented, CPU-only **mapping fixtures**, pinned to controller revision
`0d781da14e49bc51aa4fd22e4bd0718c003c23ef` (the merged #41 tree). They are not
portable run archives and do not establish #19's durable execution gate. No
model, Docker service, archived executable, or shell command was invoked.

The source records come from the production Rust controller:

- `completed.jsonl`: the two-session scenario, with streamed text deliberately
  different from the final response. It spends 26 measured tokens. Incoming
  session-two memory and evidence are exactly the first checkpoint's outputs.
- `not-dispatched.jsonl`: a model requests an exec; the controller emits its
  request and dispatch intent, then receives cancellation before `Dispatched`.
  Both intents and `tool_not_dispatched` remain visible, with effect ID 3. No
  tool result exists. Snapshot failure leaves no published checkpoint; cleanup
  completes and the primary reason remains interruption. Three tokens are known.
- `partial-failure.jsonl`: session two records partial text and thinking, then a
  connection failure. The 100-token allowance stays unknown; totals are 13 known
  plus 100 unknown, charged 113. There is no final response for request `r3`.

The fixtures retain every original `Record` field unchanged: header fields use
the #42 wrapper names, and data uses the tagged inline representation. Canonical `phase_changed`
and `request_finished` events are inserted separately, so archive sequence
numbers differ from controller sequence numbers. `mapping.json` identifies
every original observation by archive sequence and SHA-256 of the **exact JSONL
bytes including LF**. Evidence still contains original controller references,
such as `command_record: 14`; its delivered text is not rewritten.

The fixed UTC timestamp is fixture metadata, not a measured execution clock.
Model effect identities come from observed model dispatches; tool identities
come from the observed pending/dispatched effects. The custom cancellation case
captures the pending tool ID before cancellation. Unrecorded associations are
null. Real bridge capture must occur at the transition boundary, not by replaying
these fixtures or guessing IDs from timestamps.

These streams deliberately omit prepared inputs, authoritative checkpoint
publication, and canonical `run_finished`. A `controller_finished` observation
does not make them completed archives. Content references inside source records
are source data; only the owning publication contract can make them downloadable
evidence. In particular, storage's canonical content verification must not be
inferred from successful transcript decoding.

`playtime-engine/tests/observation_contract.rs` reruns all three scenarios and
compares every source field, observed effect identity, exact event digest, and
mapping entry. The protocol tests inspect the same fixtures without importing
the engine. Fixture hashes were also calculated using Python's `hashlib.sha256`.
Review contract changes before modifying these goldens; there is no automatic
fixture update mode.
