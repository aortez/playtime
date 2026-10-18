//! Replay #17's committed synthetic bytes. This never runs the controller,
//! synthetic worker, a shell command, or a model.

use std::path::Path;

use crate::{
    ArchiveWriter, BatchClass, FrozenInput, Result, StorageLimits,
    files::{reference, utf8},
};
use playtime_protocol::{
    archive::{INPUT_MANIFEST_PATH, decode_input_manifest},
    storage::{ARCHIVE_SCHEMA_VERSION, ArchiveDescriptor},
};

macro_rules! fixture_bytes {
    ($path:literal) => {
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../playtime-protocol/tests/fixtures/two-session/",
            $path
        )) as &'static [u8]
    };
}

pub struct Fixture {
    pub descriptor: ArchiveDescriptor,
    pub manifest: &'static [u8],
    pub inputs: Vec<FrozenInput>,
    pub artifacts: Vec<FrozenInput>,
    pub events: Vec<&'static [u8]>,
}

pub fn two_session() -> Result<Fixture> {
    let manifest = fixture_bytes!("inputs/manifest.json");
    let parsed = decode_input_manifest(utf8(manifest)?)?;
    let inputs = [
        (
            "inputs/configuration.json",
            fixture_bytes!("inputs/configuration.json"),
        ),
        (
            "inputs/initial-workspace.fixture",
            fixture_bytes!("inputs/initial-workspace.fixture"),
        ),
        (
            "inputs/worker/executable.fixture",
            fixture_bytes!("inputs/worker/executable.fixture"),
        ),
        (
            "inputs/worker/source.fixture",
            fixture_bytes!("inputs/worker/source.fixture"),
        ),
        (
            "inputs/worker/Cargo.lock",
            fixture_bytes!("inputs/worker/Cargo.lock"),
        ),
        ("inputs/prompt.txt", fixture_bytes!("inputs/prompt.txt")),
        ("inputs/protocol.txt", fixture_bytes!("inputs/protocol.txt")),
        ("inputs/tools.json", fixture_bytes!("inputs/tools.json")),
        (
            "inputs/model.lock.json",
            fixture_bytes!("inputs/model.lock.json"),
        ),
        (
            "inputs/workshop.lock.json",
            fixture_bytes!("inputs/workshop.lock.json"),
        ),
    ]
    .into_iter()
    .map(|(path, bytes)| FrozenInput {
        reference: reference(path, bytes),
        bytes: bytes.to_vec(),
    })
    .collect();
    let artifacts = [
        (
            "sessions/s1/workspace.fixture",
            fixture_bytes!("sessions/s1/workspace.fixture"),
        ),
        (
            "sessions/s1/memento.txt",
            fixture_bytes!("sessions/s1/memento.txt"),
        ),
        (
            "sessions/s1/evidence.txt",
            fixture_bytes!("sessions/s1/evidence.txt"),
        ),
        (
            "sessions/s2/workspace.fixture",
            fixture_bytes!("sessions/s2/workspace.fixture"),
        ),
        (
            "sessions/s2/memento.txt",
            fixture_bytes!("sessions/s2/memento.txt"),
        ),
        (
            "sessions/s2/evidence.txt",
            fixture_bytes!("sessions/s2/evidence.txt"),
        ),
    ]
    .into_iter()
    .map(|(path, bytes)| FrozenInput {
        reference: reference(path, bytes),
        bytes: bytes.to_vec(),
    })
    .collect();
    let events = fixture_bytes!("events.jsonl")
        .split_inclusive(|byte| *byte == b'\n')
        .collect();
    Ok(Fixture {
        descriptor: ArchiveDescriptor {
            schema_version: ARCHIVE_SCHEMA_VERSION,
            run_id: parsed.run_id,
            mode: playtime_core::RunMode::Simulation,
            input_manifest: reference(INPUT_MANIFEST_PATH, manifest),
        },
        manifest,
        inputs,
        artifacts,
        events,
    })
}

pub fn write_two_session(path: impl AsRef<Path>) -> Result<()> {
    let fixture = two_session()?;
    let mut writer = ArchiveWriter::create(
        path,
        fixture.descriptor,
        fixture.manifest,
        &fixture.inputs,
        StorageLimits::default(),
    )?;
    for artifact in fixture.artifacts {
        writer.publish_artifact(&artifact.reference, &artifact.bytes)?;
    }
    writer.append(&fixture.events[..8], BatchClass::Ordinary)?;
    writer.append(&fixture.events[8..15], BatchClass::Ordinary)?;
    writer.append(&fixture.events[15..], BatchClass::Finalization)?;
    writer.regenerate_result()?;
    Ok(())
}
