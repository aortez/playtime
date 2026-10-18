//! Append-only, bounded private journal. A partial file is evidence of uncertain
//! persistence and blocks reopening; there is no truncation, repair or GC path.
use crate::{Result, digest, machine::Machine, wire::*};
use playtime_core::Sha256Digest;
use playtime_local::PrivateDirectory;
use rustix::fs::{Dir, Mode, OFlags, openat};
use std::{fs::File, io::Write, os::unix::fs::MetadataExt, path::Path, sync::Arc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Fault {
    BeforeWrite,
    AfterWrite,
    AfterFileSync,
    AfterDirectorySync,
}
pub trait PublicationObserver: Send + Sync + 'static {
    fn at(&self, event: &Event, point: Fault) -> std::io::Result<()>;
}
struct NoFault;
impl PublicationObserver for NoFault {
    fn at(&self, _: &Event, _: Fault) -> std::io::Result<()> {
        Ok(())
    }
}
pub struct Journal {
    directory: PrivateDirectory,
    fd: File,
    _lock: File,
    pub(crate) machine: Machine,
    pub(crate) sequence: u64,
    previous: Sha256Digest,
    bytes: u64,
    entries: u64,
    observer: Arc<dyn PublicationObserver>,
    failed: bool,
}
impl Journal {
    /// Initialization is explicit and requires a new, empty private root.
    /// Opening a missing ledger never falls back to this operation.
    pub fn create(root: &Path) -> Result<Self> {
        Self::load(root, true)
    }
    pub fn open(root: &Path) -> Result<Self> {
        Self::load(root, false)
    }
    fn load(root: &Path, create: bool) -> Result<Self> {
        let directory = PrivateDirectory::open(root, create).map_err(|_| Failure::Storage)?;
        let lock = directory.lock().map_err(|_| Failure::Busy)?;
        let fd = File::from(
            rustix::fs::open(
                root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(|_| Failure::Storage)?,
        );
        let mut names = Vec::new();
        let mut bytes = 0u64;
        let mut entries = 0u64;
        for entry in Dir::read_from(&fd).map_err(|_| Failure::Storage)? {
            let entry = entry.map_err(|_| Failure::Storage)?;
            let name = entry.file_name().to_str().map_err(|_| Failure::Storage)?;
            if name == "." || name == ".." {
                continue;
            }
            entries = entries.checked_add(1).ok_or(Failure::Limit)?;
            if entries > STATE_RECORDS {
                return Err(Failure::Limit);
            }
            let stat = rustix::fs::statat(&fd, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|_| Failure::Storage)?;
            bytes = bytes
                .checked_add(u64::try_from(stat.st_size).map_err(|_| Failure::Storage)?)
                .ok_or(Failure::Limit)?;
            if bytes > STATE_BYTES {
                return Err(Failure::Limit);
            }
            if name != ".lock" {
                names.push(name.to_owned());
            }
        }
        names.sort();
        if create != names.is_empty() {
            return Err(Failure::Storage);
        }
        let head = if create {
            None
        } else {
            let bytes = directory
                .read("head.json", CONTROL_BYTES as u64)
                .map_err(|_| Failure::Storage)?;
            Some(serde_json::from_slice::<Head>(&bytes).map_err(|_| Failure::Storage)?)
        };
        names.retain(|name| name != "head.json");
        let mut journal = Self {
            directory,
            fd,
            _lock: lock,
            machine: Machine::default(),
            sequence: 0,
            previous: digest(&[]),
            bytes,
            entries,
            observer: Arc::new(NoFault),
            failed: false,
        };
        for name in names {
            if name != format!("{:08}.json", journal.sequence + 1) {
                return Err(Failure::Storage);
            }
            let bytes = journal
                .directory
                .read(&name, CONTROL_BYTES as u64)
                .map_err(|_| Failure::Storage)?;
            let record: JournalRecord =
                serde_json::from_slice(&bytes).map_err(|_| Failure::Storage)?;
            if record.version != VERSION
                || record.sequence != journal.sequence + 1
                || record.previous_sha256 != journal.previous
            {
                return Err(Failure::Storage);
            }
            journal
                .machine
                .apply(&record.event)
                .map_err(|_| Failure::Storage)?;
            journal.sequence = record.sequence;
            journal.previous = digest(&bytes);
            // Reconfirm complete but unacknowledged records before reuse.
            File::from(
                openat(
                    &journal.fd,
                    &name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(|_| Failure::Storage)?,
            )
            .sync_all()
            .map_err(|_| Failure::Storage)?;
            if head
                .as_ref()
                .is_some_and(|h| h.sequence == record.sequence && h.sha256 != journal.previous)
            {
                return Err(Failure::Storage);
            }
        }
        if head.as_ref().is_some_and(|h| {
            h.version != VERSION || h.sequence == 0 || h.sequence > journal.sequence
        }) {
            return Err(Failure::Storage);
        }
        journal.fd.sync_all().map_err(|_| Failure::Storage)?;
        Ok(journal)
    }
    pub fn with_observer(mut self, observer: Arc<dyn PublicationObserver>) -> Self {
        self.observer = observer;
        self
    }
    pub(crate) fn remaining(&self) -> (u64, u64) {
        (
            STATE_BYTES.saturating_sub(self.bytes + 2 * CONTROL_BYTES as u64),
            STATE_RECORDS.saturating_sub(self.entries + 2),
        )
    }
    pub(crate) fn append(&mut self, event: Event) -> Result<u64> {
        if self.failed {
            return Err(Failure::Storage);
        }
        let mut next = self.machine.clone();
        next.apply(&event)?;
        let record = JournalRecord {
            version: VERSION,
            sequence: self.sequence.checked_add(1).ok_or(Failure::Limit)?,
            previous_sha256: self.previous.clone(),
            event,
        };
        let bytes = serde_json::to_vec(&record).map_err(|_| Failure::Invalid)?;
        if bytes.len() > CONTROL_BYTES
            || self
                .bytes
                .checked_add(bytes.len() as u64 + 2 * CONTROL_BYTES as u64)
                .is_none_or(|n| n > STATE_BYTES)
            || self.entries + 2 >= STATE_RECORDS
        {
            return Err(Failure::Limit);
        }
        // No staging files: the immutable name is created once. Any incomplete
        // write remains counted and makes the next process fail closed.
        let write = (|| -> std::io::Result<()> {
            self.observer.at(&record.event, Fault::BeforeWrite)?;
            let fd = openat(
                &self.fd,
                format!("{:08}.json", record.sequence),
                OFlags::WRONLY
                    | OFlags::CREATE
                    | OFlags::EXCL
                    | OFlags::NOFOLLOW
                    | OFlags::NONBLOCK
                    | OFlags::CLOEXEC,
                Mode::from_raw_mode(0o600),
            )?;
            let mut file = File::from(fd);
            let m = file.metadata()?;
            if !m.is_file() || m.nlink() != 1 || m.uid() != rustix::process::geteuid().as_raw() {
                return Err(std::io::Error::other("unsafe journal record"));
            }
            file.write_all(&bytes)?;
            self.observer.at(&record.event, Fault::AfterWrite)?;
            file.sync_all()?;
            self.observer.at(&record.event, Fault::AfterFileSync)?;
            self.fd.sync_all()?;
            let head = Head {
                version: VERSION,
                sequence: record.sequence,
                sha256: digest(&bytes),
            };
            self.directory
                .replace("head.json", &serde_json::to_vec(&head)?)?;
            self.observer.at(&record.event, Fault::AfterDirectorySync)?;
            Ok(())
        })();
        if write.is_err() {
            self.failed = true;
            return Err(Failure::Storage);
        }
        self.bytes += bytes.len() as u64;
        self.entries += 1;
        self.sequence = record.sequence;
        self.previous = digest(&bytes);
        self.machine = next;
        Ok(self.sequence)
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Head {
    version: u32,
    sequence: u64,
    sha256: Sha256Digest,
}

#[cfg(test)]
mod tests {
    use super::*;
    fn prepared() -> Event {
        Event::Prepared {
            backend: BackendIdentity {
                backend_id: crate::random_id().unwrap(),
                guard_id: crate::random_id().unwrap(),
                boot_id: crate::random_id().unwrap(),
                unit: "fixture.service".into(),
                invocation_id: crate::random_id().unwrap(),
                cgroup: "/fixture/backend".into(),
                generation: 1,
                profile_sha256: digest(b"profile"),
            },
        }
    }
    #[test]
    fn publication_reserves_head_and_staging_at_exact_byte_and_entry_limits() {
        for over in [false, true] {
            for entry_limit in [false, true] {
                let root = std::env::temp_dir()
                    .join(format!("gj-{}", crate::random_id().unwrap().as_str()));
                let mut journal = Journal::create(&root).unwrap();
                let event = prepared();
                let size = serde_json::to_vec(&JournalRecord {
                    version: VERSION,
                    sequence: 1,
                    previous_sha256: digest(&[]),
                    event: event.clone(),
                })
                .unwrap()
                .len() as u64;
                if entry_limit {
                    journal.entries = STATE_RECORDS - 3 + u64::from(over);
                } else {
                    journal.bytes = STATE_BYTES - 2 * CONTROL_BYTES as u64 - size + u64::from(over);
                }
                assert_eq!(journal.append(event).is_err(), over);
                assert_eq!(journal.sequence, u64::from(!over));
                drop(journal);
                std::fs::remove_dir_all(root).unwrap();
            }
        }
    }
}
