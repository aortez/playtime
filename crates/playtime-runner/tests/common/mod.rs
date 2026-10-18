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
