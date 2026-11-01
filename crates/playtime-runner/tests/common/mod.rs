#![allow(dead_code)]

use playtime_core::{RunId, UtcTime};
use playtime_engine::simulation::Scenario;
use playtime_runner::{
    DriverOptions, PreparedSimulation, RunReport, VirtualClock, capture_demo_worker,
    prepare_simulation, run_prepared,
};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT: AtomicU64 = AtomicU64::new(0);
pub struct Temp {
    pub path: PathBuf,
}
impl Temp {
    pub fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "playtime-runner-tests-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self { path }
    }
    pub fn archive(&self) -> PathBuf {
        self.path.join("archive")
    }
}
impl Drop for Temp {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
pub fn scenario() -> Scenario {
    serde_json::from_str(include_str!(
        "../../../playtime-engine/tests/fixtures/two-session.json"
    ))
    .unwrap()
}
pub fn clock() -> VirtualClock {
    VirtualClock::new(UtcTime::new("2026-10-17T00:00:00Z").unwrap())
}
pub fn prepared(temp: &Temp, scenario: &Scenario) -> PreparedSimulation {
    prepare_simulation(
        temp.archive(),
        RunId::new("simulation").unwrap(),
        &serde_json::to_vec(scenario).unwrap(),
        capture_demo_worker().unwrap(),
    )
    .unwrap()
}
pub fn run(temp: &Temp, scenario: &Scenario) -> RunReport {
    let prepared = prepared(temp, scenario);
    run_prepared(
        temp.archive(),
        &prepared,
        &mut clock(),
        DriverOptions::default(),
    )
    .unwrap()
}
pub fn copy(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let next = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &next);
        } else {
            fs::copy(entry.path(), next).unwrap();
        }
    }
}

/// A confirmation of one committed prefix, as a daemon's would be.
pub struct Confirmed(
    pub playtime_protocol::storage::ArchiveIdentity,
    pub playtime_storage::ReadPrefix,
);
impl playtime_storage::ConfirmationAuthority for Confirmed {
    fn authorize(
        &self,
        archive: &playtime_protocol::storage::ArchiveIdentity,
    ) -> playtime_storage::Result<playtime_storage::ReadPrefix> {
        assert_eq!(archive, &self.0);
        Ok(self.1.clone())
    }
}

/// Queries at the record boundaries of a finished archive's committed log
/// whose sequence `keep` accepts, from the empty prefix on, sharing a reader.
pub fn boundaries(
    archive: &Path,
    keep: impl Fn(u64) -> bool,
) -> Vec<playtime_runner::ArchiveQueries> {
    use playtime_storage::{ArchiveReader, ReadPrefix, StorageLimits};
    let reader =
        std::sync::Arc::new(ArchiveReader::open(archive, StorageLimits::default()).unwrap());
    let last = reader.offline_snapshot().unwrap();
    let log = fs::read(archive.join("events.jsonl")).unwrap();
    let log = &log[..last.frontier().committed_bytes as usize];
    let mut ends = vec![0];
    for line in log.split_inclusive(|byte| *byte == b'\n') {
        ends.push(ends.last().unwrap() + line.len());
    }
    let last_sequence = ends.len() as u64 - 1;
    ends.iter()
        .enumerate()
        .filter(|(sequence, _)| keep(*sequence as u64) || *sequence as u64 == last_sequence)
        .map(|(sequence, &end)| {
            let prefix = ReadPrefix {
                sequence: sequence as u64,
                bytes: end as u64,
                sha256: playtime_storage::sha256(&log[..end]),
            };
            let snapshot = reader
                .confirmed_snapshot(&Confirmed(reader.identity().clone(), prefix))
                .unwrap();
            playtime_runner::ArchiveQueries::from_snapshot(reader.clone(), snapshot).unwrap()
        })
        .collect()
}

/// An overview that goes on from a fold of any earlier boundary, or from
/// each boundary's fold in turn, equals one read from the start, and its
/// visitor sees only the entries after the fold's.
pub fn continued_overviews_equal_whole_ones(queries: &[playtime_runner::ArchiveQueries]) {
    let whole = |queries: &playtime_runner::ArchiveQueries| {
        let mut entries = Vec::new();
        let (overview, fold) = queries
            .overview_from(None, |entry| {
                entries.push(entry.clone());
                Ok(())
            })
            .unwrap();
        (overview, fold, entries)
    };
    let last = queries.last().unwrap();
    let (expected, latest, entries) = whole(last);
    let mut chained = None;
    for earlier in queries {
        let (own, fold, _) = whole(earlier);
        let mut seen = Vec::new();
        let (overview, _) = last
            .overview_from(Some(fold), |entry| {
                seen.push(entry.clone());
                Ok(())
            })
            .unwrap();
        assert_eq!(overview.run, expected.run);
        assert_eq!(overview.request_accounting, expected.request_accounting);
        assert_eq!(overview.captures, expected.captures);
        let after = earlier.snapshot().frontier().committed_sequence as usize;
        assert_eq!(seen, entries[after..]);
        let (overview, fold) = earlier.overview_from(chained, |_| Ok(())).unwrap();
        assert_eq!(overview.run, own.run);
        assert_eq!(overview.request_accounting, own.request_accounting);
        assert_eq!(overview.captures, own.captures);
        chained = Some(fold);
    }
    // A fold of a later prefix doesn't go on to an earlier one.
    let first = &queries[0];
    assert!(!first.continues(&latest));
    assert!(first.overview_from(Some(latest), |_| Ok(())).is_err());
}
