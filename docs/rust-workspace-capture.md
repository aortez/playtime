# Stopped-workspace capture (D2, #54)

`playtime-docker::capture` turns an owned, stopped workshop into a bounded
`SnapshotCandidate`. It does not publish a checkpoint or enable production Docker
execution. #24 D3 still supplies the controller effect adapter, durable dispatch
barriers, archive preparation, daemon recipe capability, and run-and-view gate.
#20, #21 and #22 remain open integration work. The Python workflow is retained.

## Ownership and completion

Capture requires D1's non-serializable `Quiescent<'_>` exclusive sandbox borrow.
The original total stop/capture deadline is retained (at most 80 seconds, of which
at most 20 seconds may be spent quiescing). It is never renewed by another capture
attempt. The holder/volume identities and sole remaining mount are checked both
before exporting and before returning a candidate. The holder stays mounted and a
new session cannot start while the capture borrow exists.

Both helpers run through the selected Docker transport, the locked image's
`/usr/local/bin/python3.12 -I -S -B`, workdir `/`, and uid/gid 1000. Literal source
is embedded from `crates/playtime-docker/src/capture/*.py`; limits arrive separately
on stdin. There is no runtime source lookup, model-selected executable, host shell,
Cargo invocation, image rebuild, or lock update. The root lifetime timer remains
separate. Helper digests accompany the candidate and are recorded in the frozen
worker build record. The worker compares them to its compiled helper bytes before
execution. This adds no real-workshop worker capability.

Before helper dispatch, the holder is fenced. Only explicit non-dispatch evidence
or complete successful transport clears the pending-helper state. Lost replies,
nonzero/uncertain exits, cancellation, timeout, and dropped futures retain it.
Neither local CLI reaping nor another agent-absence lookup can clear that state.
Final cleanup force-removes the exact owned holder and verifies absence before
volume removal, under D1's single aggregate 20-second cleanup budget. If cleanup
cannot establish cessation, residual ownership is reported. It is safe to lose a
candidate; it is not safe to resume using an uncertain exporter.

Successful capture requires complete tar and memory replies, zero helper exits,
strict validation, an unexpired deadline, final identity/mount checks, and remaining
archive capacity. Parse failures return no candidate. An otherwise successful
export is never interpreted as a storage acknowledgment.

## Export format and independent validation

The stdlib-only exporter walks `/work` descriptor-relatively. It opens directories
and regular files with no-follow/nonblocking flags, compares inode/type/mode/size/
mtime/ctime/link-count before and after reads, checks parent directory identities,
and checks the inventory again before finishing. Quiescence provides consistency;
unexpected mutation fails the attempt. Entries and pending inventory names share
one count bound. Unreadable files, FIFO/device/socket entries and invalid UTF-8 fail
explicitly. Owner-readable 0600 files work without chmod or extra capabilities.

Uncompressed POSIX PAX tar uses constructed headers and streaming writes. Logical
names sort by UTF-8 bytes; directories precede descendants. There is no root
member. uid/gid become 1000, owner/group names become empty, and mtime becomes zero;
permission bits, including executable/special mode bits, remain metadata. Sparse
filesystem files are read densely under logical-byte limits. No sparse tar format
is accepted. These timestamp/ownership changes are deliberate differences from
Python's earlier Docker-copy artifacts; archive bytes are not a continuation image.

Regular files retain exact bytes, size, mode and SHA-256. Directories and links
retain metadata only. Symlinks preserve bounded targets, even absolute or escaping
targets, without following them. Later hardlinks name the first regular inode's
logical path. They never become extra retained file contents or permission to
extract/continue a workspace. Seed import is unchanged and remains files/directories.

The Rust validator uses `Read`, explicit 512-byte blocks and separate incremental
raw-tar and file hashes. It uses no tar extraction library. It checks unsigned
checksums, octal arithmetic, normalized metadata, payload sizes/padding, exact PAX
record lengths, UTF-8 and duplicate fields. Only `path`/`linkpath` per-entry PAX
fields are accepted, with one extension per member and a 16 KiB limit. Effective
names are validated after overrides. Names must be relative, nonempty, at most
4096 UTF-8 bytes/32 components, and contain no NUL/empty/dot/dot-dot components.
Directories canonicalize one trailing slash. Parents must already be directories;
hardlinks must reference earlier regular members. Global/GNU/sparse extensions,
forward links/cycles, duplicate/conflicting names, truncation and nonzero tails
fail. Two zero end blocks are required; trailing zero padding is bounded to 10 KiB
and the complete stream must be block-aligned.

The transport buffers at most the frozen tar cap, never unlimited stdout. The
validator streams over that bounded buffer on a blocking task, checks cancellation
between reads, and owns separate bounded tar/body staging bytes. During validation
both transport and validation tar buffers may coexist; the default maximum payload
storage is 192 MiB (two 64 MiB tar buffers plus 64 MiB retained content), plus bounded
index/metadata, memory and allocator overhead. The 8 MiB smoke profile lowers the
retained-content part to 8 MiB. No staging file is opened on the host. Individual
bodies use controller-generated numeric slots, so no workspace name reaches a host
filesystem API. Public `ValidatedWorkspace` is only a parser result;
`SnapshotCandidate` construction additionally requires the live capture checks.

## Capacity and memory

`CaptureProfile::prepare` freezes requested limits and clamps them to the caller's
workspace and storage per-object/manifest bounds. Defaults are 4096 entries, 64 MiB
tar, 64 MiB individual/aggregate regular content, 4 MiB candidate index, and at most
65536 outgoing memory bytes. The transport retains at most 16 KiB stderr. The smoke
workspace limits individual and aggregate regular contents to 8 MiB. Physical tmpfs
allocation is never used as a logical-byte bound.

The integrating owner supplies `ArchiveUsage`: all frozen inputs and earlier
checkpoints **plus reserved publication overhead**. Overhead includes the canonical
runner checkpoint index, source map, incoming memory/evidence, event envelopes and
any other objects the runner will add. Count references/events/log bytes as well as
content bytes. Capture checks the actual tar plus duplicated individual contents,
its candidate index, outgoing memory and their references against the remainder;
checked arithmetic rejects overflow. Preparation checks event/log availability and
per-object compatibility. D3 must derive usage/reserves from authoritative writer
state and update cumulative usage between checkpoints. This API is not an admission
reservation, and fitting the workspace limit does not promise that every tree fits
the archive. The writer independently enforces its actual limits; no global storage
default has been raised.

The separate trusted memory helper reads the actual `MEMENTO.md` as uid 1000 after
quiescence. Loaded empty text, missing, unreadable/nonregular, oversized, and binary
are distinct. Text remains exact, with no final-handoff substitution. Links are
captured as inert metadata; the memory helper refuses symlinks instead of following
outside targets. A regular hardlinked memento is readable. Rust cross-checks the
memory status and exact loaded bytes against the tar's regular body (resolving only
an internal earlier hardlink). Unexpected differences fail the candidate. An
unreadable regular entry also fails tar capture, so its memory error cannot turn a
partial workspace into a successful candidate. Incoming delivered memory remains
the runner's separate recorded delivery.

## Publication boundary and evidence

The runner's checkpoint encoder now accepts validated regular bodies separately
from the opaque workspace archive. The scripted adapter still supplies its existing
JSON fixture format. A synthetic transport fixture passes a real capture candidate
through this format-independent mapping and the existing storage thread. It checks
exact file bytes, metadata-only links, incoming/outgoing memory distinctions, and
three writer failure points. A completed handoff followed by publication failure
advertises only the earlier checkpoint. A marker renamed before failure remains
offline evidence, not a promoted live receipt. A delayed receipt keeps the writer
owned and advances the live frontier only when actually delivered. No failed writer
is retried. Production composition remains D3.

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
PLAYTIME_DOCKER_TESTS=1 python3 -m unittest discover -s tests -v
./scripts/test-rust-docker-e2e.sh --case sandbox
./scripts/test-rust-docker-e2e.sh --case checkpoint
./scripts/test-rust-worker-package.sh
```

The opt-in checkpoint gate uses the already-present locked image and no model.
It matches the reviewed reference bytes in that image, stops a real background
writer, verifies repeated captures are stable, preserves owner-only modes, Unicode,
empty entries, hardlinks, escaping symlinks and outgoing memory, poisons helper
imports/cwd/PYTHONPATH/user-site, and verifies unreadable/oversized/binary memory and unprivileged permission denial. A FIFO capture
fails without replacing prior saved evidence. Exact owned cleanup is recorded.
Invocation fails if the environment is unavailable; it never substitutes a fake.
The gate is candidate evidence, not checkpoint publication or UI production proof.
The CPU suite separately injects mutation, capacity, malformed tar, cancellation,
dropped futures and publication failures. The worker-package gate verifies helper
bytes exist in the actual static executable and their recorded digests match.
