mod common;
use common::*;
use playtime_backend_guard::{Journal, capability, fixture::Mode, wire::*};
use std::{
    fs::{self, OpenOptions},
    os::unix::fs::{OpenOptionsExt, symlink},
};

#[tokio::test]
async fn private_state_never_contains_dispatch_capability_and_detects_corrupt_tail() {
    let mut h = Harness::new(Mode::Terminal).await;
    let client = h.start(true, None).await;
    let lease = lease(&h.identity);
    let cap = capability().unwrap();
    client
        .control(Command::Acquire {
            lease: lease.clone(),
            capability: cap.clone(),
        })
        .await
        .unwrap();
    let (stream, _) = client
        .dispatch(request(&lease, 1), cap.clone(), b"x")
        .await
        .unwrap();
    finish(stream).await;
    h.guards[0].kill();
    for entry in fs::read_dir(h.root.join("state")).unwrap() {
        let bytes = fs::read(entry.unwrap().path()).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains(cap.expose()));
    }
    assert!(Journal::open(&h.root.join("state")).is_ok());
    // A complete journal entry cannot be silently discarded by deleting its
    // last name: the separately durable head still binds its sequence/hash.
    fs::remove_file(h.root.join("state/00000004.json")).unwrap();
    assert!(Journal::open(&h.root.join("state")).is_err());
}

#[tokio::test]
async fn staging_growth_and_record_count_are_included_in_the_fixed_budget() {
    for bytes in [true, false] {
        let mut h = Harness::new(Mode::Terminal).await;
        h.start(true, None).await;
        h.guards[0].kill();
        let state = h.root.join("state");
        if bytes {
            let file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(state.join(".unfinished-staging"))
                .unwrap();
            file.set_len(STATE_BYTES).unwrap();
        } else {
            for i in 0..STATE_RECORDS {
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(state.join(format!(".staging-{i}")))
                    .unwrap();
            }
        }
        assert!(matches!(Journal::open(&state), Err(Failure::Limit)));
        assert!(state.join("00000001.json").is_file());
        assert!(state.join("head.json").is_file());
    }
}

#[tokio::test]
async fn unknown_version_duplicate_fields_partial_record_and_special_files_block() {
    for case in [
        "version",
        "duplicate",
        "partial",
        "fifo",
        "symlink",
        "hardlink",
        "head_missing",
        "orphan",
    ] {
        let mut h = Harness::new(Mode::Terminal).await;
        h.start(true, None).await;
        h.guards[0].kill();
        let state = h.root.join("state");
        let record = state.join("00000001.json");
        match case {
            "version" => {
                let text = fs::read_to_string(&record).unwrap().replacen(
                    "\"version\":1",
                    "\"version\":2",
                    1,
                );
                fs::write(&record, text).unwrap();
            }
            "duplicate" => {
                let text = fs::read_to_string(&record).unwrap().replacen(
                    "\"version\":1",
                    "\"version\":1,\"version\":1",
                    1,
                );
                fs::write(&record, text).unwrap();
            }
            "partial" => {
                OpenOptions::new()
                    .write(true)
                    .open(&record)
                    .unwrap()
                    .set_len(12)
                    .unwrap();
            }
            "fifo" => {
                fs::remove_file(&record).unwrap();
                rustix::fs::mkfifoat(
                    rustix::fs::CWD,
                    &record,
                    rustix::fs::Mode::from_raw_mode(0o600),
                )
                .unwrap();
            }
            "symlink" => {
                fs::rename(&record, state.join("source")).unwrap();
                symlink(state.join("source"), &record).unwrap();
            }
            "hardlink" => {
                fs::hard_link(&record, state.join("source")).unwrap();
            }
            "head_missing" => {
                fs::remove_file(state.join("head.json")).unwrap();
            }
            "orphan" => {
                fs::write(state.join(".unknown"), b"incomplete").unwrap();
            }
            _ => unreachable!(),
        }
        let started = std::time::Instant::now();
        assert!(Journal::open(&state).is_err(), "accepted {case}");
        assert!(
            started.elapsed().as_secs() < 2,
            "special file open must be nonblocking"
        );
    }
}
