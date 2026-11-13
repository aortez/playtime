use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt, symlink},
    path::PathBuf,
};

use playtime_core::{RunMode, Sha256Digest, StopReason};
use playtime_engine::{
    chat::ConversationConfig,
    compaction::CompactionSettings,
    simulation::{Scenario, SequentialIds},
    *,
};
use playtime_harness::{Session, Turn};
use playtime_local::Token;
use serde_json::json;

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "playtime-chat-{}",
            Token::generate().unwrap().expose()
        )))
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn config() -> ConversationConfig {
    let fixture: Scenario = serde_json::from_str(include_str!(
        "../../playtime-engine/tests/fixtures/two-session.json"
    ))
    .unwrap();
    let mut parameters = fixture.parameters.common();
    parameters.limits.context_tokens = 16384.try_into().unwrap();
    parameters.limits.tokens_per_call = 512.try_into().unwrap();
    parameters.limits.generated_tokens = 10000.try_into().unwrap();
    parameters.limits.output_bytes = 2048.try_into().unwrap();
    ConversationConfig {
        schema_version: 1,
        mode: RunMode::Simulation,
        tool_policy: ToolPolicy::Simulation,
        model: Sha256Digest::new("1".repeat(64)).unwrap(),
        workspace: Sha256Digest::new("2".repeat(64)).unwrap(),
        instructions: "Answer when finished.".into(),
        inventory: "Owned workshop.".into(),
        parameters,
        compaction: CompactionSettings {
            allow_finish_run: false,
            summary_tokens: 256.try_into().unwrap(),
            ..Default::default()
        },
    }
}

fn complete(turn: &mut Turn<SequentialIds>, now: &mut u64, outcome: Outcome) {
    let effect_id = turn.pending().unwrap().id;
    turn.advance(*now, Input::Dispatched { effect_id }).unwrap();
    assert_eq!(turn.pending().unwrap().id, effect_id);
    *now += 1;
    turn.advance(*now, Input::Complete { effect_id, outcome })
        .unwrap();
}
fn answer(turn: &mut Turn<SequentialIds>, now: &mut u64, text: &str) {
    complete(
        turn,
        now,
        Outcome::ModelCompleted {
            response: ModelResponse {
                message: json!({"role":"assistant", "content":text}),
                done: true,
                generated_tokens: Some(10),
                prompt_tokens: None,
            },
            finish_reason: "stop".into(),
        },
    );
    assert!(matches!(turn.pending().unwrap().kind, EffectKind::Cleanup));
    complete(turn, now, Outcome::Cleaned(vec![]));
}
fn read_records(root: &Root) -> Vec<Record> {
    let mut records = Vec::new();
    for entry in fs::read_dir(&root.0).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "jsonl") {
            for line in fs::read_to_string(path).unwrap().lines() {
                records.push(serde_json::from_str::<Record>(line).unwrap());
            }
        }
    }
    records.sort_by_key(|r| r.sequence);
    records
}

#[test]
fn saved_turn_reopens_without_a_service_and_keeps_incremental_private_history() {
    let root = Root::new();
    let config = config();
    let session = Session::create(&root.0, config.clone()).unwrap();
    let mut turn = session
        .begin("Find the issue.".into(), SequentialIds, 0)
        .unwrap();
    assert!(root.0.join("head.json").exists());
    assert!(!read_records(&root).is_empty());
    let mut now = 0;
    complete(&mut turn, &mut now, Outcome::WorkspaceStarted);
    answer(
        &mut turn,
        &mut now,
        "The parser is missing a delimiter check.",
    );
    let first_id = turn.id().clone();
    let session = turn.finish().unwrap();
    let checkpoint = session.checkpoint().unwrap().clone();
    drop(session);

    let session = Session::open(&root.0, &config).unwrap();
    assert_eq!(session.checkpoint(), Some(&checkpoint));
    let mut turn = session
        .begin("Fix it.".into(), SequentialIds, 100_000)
        .unwrap();
    assert_ne!(*turn.id(), first_id);
    now = 100_000;
    complete(&mut turn, &mut now, Outcome::WorkspaceStarted);
    let EffectKind::Model(request) = &turn.pending().unwrap().kind else {
        panic!("expected inference")
    };
    assert!(
        request
            .messages
            .iter()
            .any(|m| m["content"] == "The parser is missing a delimiter check.")
    );
    assert_eq!(request.messages.last().unwrap()["content"], "Fix it.");
    answer(&mut turn, &mut now, "Fixed.");
    drop(turn.finish().unwrap());

    let records = read_records(&root);
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.sequence, index as u64 + 1);
    }
    assert_eq!(
        records
            .iter()
            .filter(|r| r.kind == "controller_finished")
            .count(),
        2
    );
    assert_eq!(fs::metadata(&root.0).unwrap().mode() & 0o777, 0o700);
    for entry in fs::read_dir(&root.0).unwrap() {
        assert_eq!(entry.unwrap().metadata().unwrap().mode() & 0o777, 0o600);
    }
}

#[test]
fn a_crash_fences_the_whole_unfinished_turn_without_replaying_it() {
    let root = Root::new();
    let config = config();
    let mut turn = Session::create(&root.0, config.clone())
        .unwrap()
        .begin("Edit a file.".into(), SequentialIds, 0)
        .unwrap();
    let mut now = 0;
    complete(&mut turn, &mut now, Outcome::WorkspaceStarted);
    complete(
        &mut turn,
        &mut now,
        Outcome::ModelCompleted {
            response: ModelResponse {
                message: json!({"role":"assistant", "tool_calls":[{"function":{"name":"write_file", "arguments":{"path":"a", "content":"changed"}}}]}),
                done: true,
                generated_tokens: Some(10),
                prompt_tokens: None,
            },
            finish_reason: "tool_calls".into(),
        },
    );
    assert!(
        matches!(&turn.pending().unwrap().kind, EffectKind::Tool(request) if request.name == "write_file")
    );
    let effect_id = turn.pending().unwrap().id;
    turn.advance(now, Input::Dispatched { effect_id }).unwrap();
    drop(turn);
    let error = Session::open(&root.0, &config).err().unwrap();
    assert!(error.to_string().contains("automatic recovery is disabled"));
    assert!(read_records(&root).iter().any(|r| r.kind == "tool_request"));
}

#[test]
fn a_failed_record_write_stops_work_but_still_exposes_cleanup() {
    let root = Root::new();
    let config = config();
    let mut turn = Session::create(&root.0, config.clone())
        .unwrap()
        .begin("Fix it.".into(), SequentialIds, 0)
        .unwrap();
    let next_sequence = read_records(&root).last().unwrap().sequence + 1;
    let blocked = root
        .0
        .join(format!("events-{}-{next_sequence:020}.jsonl", turn.id()));
    fs::write(&blocked, b"existing evidence").unwrap();
    let effect_id = turn.pending().unwrap().id;
    turn.advance(0, Input::Dispatched { effect_id }).unwrap();
    assert!(
        turn.advance(
            1,
            Input::Complete {
                effect_id,
                outcome: Outcome::WorkspaceStarted
            }
        )
        .is_err()
    );
    assert!(matches!(turn.pending().unwrap().kind, EffectKind::Cleanup));
    let mut now = 1;
    complete(&mut turn, &mut now, Outcome::Cleaned(vec![]));
    assert_eq!(
        turn.result().unwrap().reason,
        StopReason::InfrastructureFailure
    );
    assert!(turn.finish().is_err());
    assert_eq!(fs::read(&blocked).unwrap(), b"existing evidence");
    assert!(Session::open(&root.0, &config).is_err());
}

#[test]
fn cancellation_persists_unknown_usage_but_cannot_advance_the_saved_head() {
    let root = Root::new();
    let config = config();
    let mut turn = Session::create(&root.0, config.clone())
        .unwrap()
        .begin("Fix it.".into(), SequentialIds, 0)
        .unwrap();
    let mut now = 0;
    complete(&mut turn, &mut now, Outcome::WorkspaceStarted);
    let effect_id = turn.pending().unwrap().id;
    turn.advance(now, Input::Dispatched { effect_id }).unwrap();
    turn.advance(now, Input::Cancel).unwrap();
    complete(&mut turn, &mut now, Outcome::Cleaned(vec![]));
    let result: RunResult = serde_json::from_slice(
        &fs::read(root.0.join(format!("result-{}.json", turn.id()))).unwrap(),
    )
    .unwrap();
    assert_eq!(result.usage.unknown_requests, 1);
    assert_eq!(result.usage.unknown_reserved_tokens, 512);
    assert!(turn.finish().is_err());
    assert!(Session::open(&root.0, &config).is_err());
}

#[test]
fn resume_refuses_changed_configuration_and_unsafe_private_files() {
    let root = Root::new();
    let config = config();
    drop(Session::create(&root.0, config.clone()).unwrap());
    let mut changed = config.clone();
    changed.workspace = Sha256Digest::new("3".repeat(64)).unwrap();
    assert!(Session::open(&root.0, &changed).is_err());
    let path = root.0.join("head.json");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(Session::open(&root.0, &config).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::rename(&path, root.0.join("real-head.json")).unwrap();
    symlink("real-head.json", path).unwrap();
    assert!(Session::open(&root.0, &config).is_err());
}

#[test]
fn a_large_checkpoint_is_immutable_and_does_not_hit_the_small_head_replace_limit() {
    let root = Root::new();
    let mut config = config();
    config.parameters.limits.context_tokens = 2_000_000.try_into().unwrap();
    config.parameters.limits.log_bytes = 16_000_000.try_into().unwrap();
    let mut turn = Session::create(&root.0, config.clone())
        .unwrap()
        .begin("Investigate.".into(), SequentialIds, 0)
        .unwrap();
    let mut now = 0;
    complete(&mut turn, &mut now, Outcome::WorkspaceStarted);
    answer(&mut turn, &mut now, &"x".repeat(1_100_000));
    let checkpoint_path = root.0.join(format!("checkpoint-{}.json", turn.id()));
    drop(turn.finish().unwrap());
    assert!(fs::metadata(checkpoint_path).unwrap().len() > 1024 * 1024);
    let session = Session::open(&root.0, &config).unwrap();
    let mut turn = session
        .begin("Continue.".into(), SequentialIds, 100_000)
        .unwrap();
    now = 100_000;
    complete(&mut turn, &mut now, Outcome::WorkspaceStarted);
    answer(&mut turn, &mut now, "Done.");
    drop(turn.finish().unwrap());
    assert!(Session::open(&root.0, &config).is_ok());
}
