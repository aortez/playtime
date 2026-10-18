# Portable Rust archive storage

Tracking: [#33](https://github.com/aortez/playtime/issues/33), the independent
storage slice of [#19](https://github.com/aortez/playtime/issues/19).
Branch `33-archive-storage` starts at foundation commit
`de1647ad54e7d842b365b60ebf4c95f78936cf20`. It does not include the independent
`18-rust-controller` branch. Stack its review against `17-rust-foundation`.

`playtime-storage` implements synchronous, bounded filesystem I/O, verification,
and publication. Its dependencies are core/protocol, the pinned Serde versions,
SHA-256 (`sha2` 0.11.1, as used by #18), and `rustix` 1.1.4 for safe,
descriptor-relative Linux filesystem operations. Wire DTOs are in
`playtime_protocol::storage`. No runtime, engine, HTTP, SQLite, model, Docker,
worker, or GUI is needed. The Rust filesystem implementation is currently Linux
only; the archive bytes and their identities do not depend on host paths.

## Demonstration

From the repository root, choose a directory that does not already exist:

```sh
cargo run --locked -p playtime-storage --example archive_fixture -- --output /tmp/playtime-archive-fixture
```

The example publishes #17's exact synthetic two-session bytes, closes the
writer, reopens and verifies the archive, and prints `operation: fixture_replay`.
It reports 16 events, two checkpoints, complete cleanup, 10 known generated
tokens and 100 reserved unknown tokens (110 charged). It executes neither a
controller nor the synthetic worker. The fixture records that worker execution
is unavailable. The folder-only copy test removes the original folder and
verifies every frozen input and referenced artifact from the copy.

This is #33's storage gate. #19 must separately execute the reviewed controller,
persist that execution, and pass the same detached inspection gate.

## Layout and authority

| Path | Contract |
| --- | --- |
| `archive.json` | Immutable layout version 1, run ID, mode, exact input-manifest reference |
| `inputs/manifest.json` | Existing independently versioned input manifest; every reference must verify before preparation returns |
| `inputs/` | Frozen configuration, seed workspace, worker executable/source/lock, prompts and identities |
| `events.jsonl` | Exact supplied UTF-8 JSON objects plus one LF per record |
| `events.commit.json` | Commit version 1, run ID, last sequence, prefix byte length and SHA-256 including LFs |
| `sessions/`, `artifacts/` | Immutable bytes; evidence only when referenced by committed known events |
| `result.json` | Optional exact copy of the committed `run_finished` line; readers never consult it |
| `.staging/` | Unpublished temporary objects; diagnostics only |
| `.writer.lock` | Stable advisory lock inode; never replace/delete it to bypass ownership |

Archive, event, manifest, and commit-marker versions are independent. Unsupported
majors fail before their bodies are interpreted. Creation descriptors and local
I/O settings reject unknown fields. Opening an archive performs no writes or
upgrades. The marker's empty prefix has sequence and length zero and the SHA-256
empty digest. The new [format goldens](../crates/playtime-storage/tests/fixtures/README.md)
were checked independently with Python stdlib hashing; #17's goldens are unchanged.

`ArchiveWriter::create` requires a new directory, its exact descriptor/manifest,
and exactly the manifest's referenced objects. It returns only after verifying
the stored input bytes and publishing metadata. A per-archive exclusive lock
lasts until the writer drops, including after a failure. This lock is separate
from #20's data-root admission lock. An archive's existence or successful
preparation does not grant permission to execute anything.
Dropping a lock explicitly unlocks it before closing its descriptor, so a
concurrent process spawn cannot temporarily retain ownership through an
inherited descriptor awaiting `exec`.

## Publication and failure

Immutable objects are written beneath `.staging/` on the same filesystem, synced,
renamed with Linux `RENAME_NOREPLACE`, then their destination and staging
directories are synced. Newly created ancestors are synced too. Publishing an
existing path checks its exact identity; different bytes cannot replace it.
Only sessions/artifacts can be added after input preparation.

`append` takes a bounded batch of exact JSONL byte slices and an explicit
`Ordinary` or `Finalization` classification. It checks the run ID, contiguous
sequences from 1, versions and known payloads. Sequence 1 must be `run_started`
bound to the descriptor. Unknown event kinds and extra fields retain their exact
raw bytes. The library does not choose timestamps or interpret command strings.
Canonical request settlements cannot repeat; a terminal summary must equal the
checked settlement totals and name the last published checkpoint. Nothing may
append after `run_finished`, which requires a finalization-class batch.

Before appending, all content referenced by the batch is verified. Files and
every ancestor directory of those references are synced before checkpoint/event
publication. The writer then appends all event bytes, syncs the event file,
writes and syncs a temporary marker, atomically replaces `events.commit.json`,
and syncs the root and staging directories. Only after all these operations
succeed does it update its state and return `CommittedFrontier`.

The receipt includes the run, exact archive/manifest identity, exact marker
identity, and prefix sequence/length/hash. Its library type cannot be constructed
by deserializing a discovered marker. A live reader takes a receipt explicitly;
it never inspects the current marker to choose a newer boundary. A marker may
be visible even when its directory sync failed and no receipt was returned.

Publication failure stops the writer and sets `outcome_uncertain` on its error.
I/O errors also stop it during preflight. Failed publication cannot be retried by
changing the observer, publishing another object, or appending the same batch.
Previously returned receipts still name readable prefixes. Partial event lines,
complete uncommitted suffixes, staging files, and orphan artifacts are retained
for diagnosis. A failure writing derived `result.json` cannot revoke a prior
terminal receipt. Missing/stale/fabricated result files never create completion.

Offline inspection takes a shared lock while selecting and validating the
recovered marker, refusing an active writer. After confirmed process exit it can
observe an old or newly published valid marker; that does not prove whether the
failed caller received acknowledgment. Service ownership, reattachment and
controlled reconciliation belong to #19/#20. There is no writer-resume or
automatic repair API here. Readers never truncate/promote a suffix, skip a gap,
or repair a bad committed hash. When no terminal event commits, inspection says
`incomplete_evidence`; usage then describes observed canonical settlements, not
an assurance that every attempted request is accounted for.

Durability assumes a filesystem/device that honor [file and directory fsync](https://man7.org/linux/man-pages/man2/fsync.2.html)
and [atomic rename](https://man7.org/linux/man-pages/man2/rename.2.html). Tests cover
injected I/O errors and killed processes; they do not prove arbitrary hardware
power-loss safety. The owner must protect the archive directories from hostile
host-side mutation. Models never have host access to these archives.

## Reads, containment and bounds

`ArchiveReader::open` verifies `archive.json`, exact manifest bytes, and all
frozen input references. Worker executable bytes are verified independently of
the viewer build. `WorkerAvailability.execution_checked` remains false: storage
does not test the archived ABI, platform requirements, executability, installed
dependencies, or rebuildability. Recorded platform requirements remain visible.

`live_snapshot(receipt)` or `offline_snapshot()` returns a fixed `Snapshot`.
An `EventQuery` carries its archive/run/manifest identity, exclusive
`after_sequence`, inclusive `through_sequence`, and a limit of 1–1,000 (default
200). Every page holds the identical frontier. A wrong identity or changed
frontier fails; a cursor beyond it returns `cursor_ahead`. `next_after_sequence`
and `has_more` support the next page, including an empty log. There is no
retention, compaction, cursor expiry, filtering, or HTTP encoding in this crate.

Every snapshot/page validates the complete selected prefix, including sequence
continuity, exact hash, terminal accounting, and referenced content. Raw event
bytes and their individual SHA-256 digests are returned with parsed envelopes.
This is a bounded linear scan, without a persistent index; later indexes must
preserve these verification/authority rules. A page may return fewer records
than its requested count to stay within its byte bound, without skipping one.

`read_content` accepts only exact references from that snapshot's input manifest
or known events. `uncommitted_objects` lists diagnostic names separately,
including staging and orphan objects; these are never ordinary evidence.
`diagnostic_event_bytes` reads a bounded unverified byte range and issues no
trusted event cursor, even if prefix validation failed.

Every path component is opened relative to a held directory descriptor using
`O_NOFOLLOW`. Roots and their ancestors cannot contain symlinks or `..`.
Referenced files must be regular files with a single hard link. Nonblocking
opens prevent FIFOs from hanging inspection; devices, sockets, directories in
place of files, and symlinks are rejected. Paths have a 4,096-byte/32-component
bound. No archive extraction or execution occurs.

Default `StorageLimits` are 64 MiB of event bytes, 1 MiB per record (including LF),
4 MiB per batch/page, 64 MiB per content object, 4 MiB per manifest, 512 MiB of
unique verified content, 10,000 references and 100,000 events. Inventory has a
bounded entry count too. Metadata has a fixed 65,536-byte read bound. All limits
are explicit and checked; reader limits may be stricter than writer limits.

Ordinary event writes fit within `max(0, event_log_bytes - 65_536)`; explicit
finalization batches may use the remaining log allowance. There is no separate
unbounded terminal channel. Batches count actual envelope and LF bytes, never a
controller estimate. Storage has no timer or background task. #19 will target
250 ms or 64 KiB batches, commit before dependent effects/lifecycle acknowledgments,
and commit larger valid records individually. Slow filesystem operations can
exceed that scheduling target; the supervising driver owns operation deadlines.

Errors expose `malformed_data`, `unsupported_version`, `invalid_cursor`,
`cursor_ahead`, `archive_corrupt`, `content_mismatch`, `writer_busy`,
`limit_exceeded`, or `io_error`, plus readable context and uncertain-publication
status. No caller should branch on message text.

## Validation and next integration

Tests exercise folder-only copies, immutable publication, exact hashes, malformed
known and preserved unknown events, strict sequence checks, fixed pagination
while appending, limits/finalization, wrong cursors, tampering, links/special
files, writer contention, and optional results. Failure observers surround real
write/sync/rename calls, including short/zero writes and failures before/after
publication boundaries. A parent test kills subprocess writers during partial
event writes, before/after marker rename, and after directory sync, then checks
lock release and recovered evidence. The ignored `crash_child` harness is
explicitly executed by that parent test; it is not an untested scenario.

Run the repository's formatting, locked Clippy/tests, and Python unittest suite.
No live services, GPU, Docker, or desktop are used. The existing workflow already
covers all workspace crates and example compilation.

Local validation: formatting and locked Clippy with warnings denied passed;
55 workspace Rust tests passed, including the parent crash tests and the lock
inheritance regression. The Python suite ran 91 tests: 85 passed and six Docker
tests were explicitly skipped. The documented fixture example also completed
successfully. No existing input/event/schema fixture changed.

#19 owns versioned controller observations, archive sequence/digest mappings,
retained outboxes, dispatch barriers, actual checkpoint manifests/indexes,
interrupted-run reconciliation and legacy readers. #20 owns admission SQLite
transactions, worker ownership transfer and service transport. Neither can use
an unconfirmed marker or an in-memory result as durable completion authority.

## Durable driver integration (#42)

`open_prepared` is a narrow empty-archive handoff: exclusive writer lock, exact
expected identity, complete verified inputs, zero marker, empty event file, and
durability confirmation before a receipt. It rejects nonempty logs and tails;
it is not a resume or execution-admission API. `PublicationObserver` is `Send`
so a storage owner can live on a dedicated writer thread.

Known-reference traversal now verifies external controller-observation JSON and
versioned checkpoint indexes/source maps with their individual content leaves.
Only defined traversal roles are followed. Indexes and maps obey manifest bounds;
all transitive contents obey reference/aggregate bounds. Maps must identify exact
preceding committed observation lines including LF, never records newly staged
in the same append. Existing synthetic fixtures and their bytes are unchanged.

See [rust-archive-driver.md](rust-archive-driver.md) for executing simulations,
late receipt handling, and normalized copied-folder queries. Recovery and legacy
readers remain separate #43/#44 work.
