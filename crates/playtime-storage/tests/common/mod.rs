#![allow(dead_code)]

use playtime_storage::{
    ArchiveReader, ArchiveWriter, BatchClass, StorageLimits,
    fixture::{self, Fixture},
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
        let parent = std::env::temp_dir().join(format!(
            "playtime-storage-tests-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&parent).unwrap();
        Self { path: parent }
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

pub fn writer(temp: &Temp, limits: StorageLimits) -> (ArchiveWriter, Fixture) {
    let fixture = fixture::two_session().unwrap();
    let writer = ArchiveWriter::create(
        temp.archive(),
        fixture.descriptor.clone(),
        fixture.manifest,
        &fixture.inputs,
        limits,
    )
    .unwrap();
    (writer, fixture)
}
pub fn artifacts(writer: &mut ArchiveWriter, fixture: &Fixture) {
    for artifact in &fixture.artifacts {
        writer
            .publish_artifact(&artifact.reference, &artifact.bytes)
            .unwrap();
    }
}
pub fn completed(temp: &Temp) -> ArchiveReader {
    fixture::write_two_session(temp.archive()).unwrap();
    reader(temp)
}
pub fn reader(temp: &Temp) -> ArchiveReader {
    ArchiveReader::open(temp.archive(), StorageLimits::default()).unwrap()
}
pub fn all_events(writer: &mut ArchiveWriter, fixture: &Fixture) {
    artifacts(writer, fixture);
    writer
        .append(&fixture.events, BatchClass::Finalization)
        .unwrap();
}
pub fn changed(bytes: &[u8], change: impl FnOnce(&mut serde_json::Value)) -> Vec<u8> {
    let mut value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    change(&mut value);
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    bytes
}
pub fn copy_directory(source: &Path, destination: &Path) {
    fs::create_dir(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_directory(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}
