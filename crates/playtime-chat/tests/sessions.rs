use playtime_chat::sessions::{State, catalog, select};
use playtime_core::{RunMode, Sha256Digest};
use playtime_engine::{
    Input, ModelResponse, Outcome, ToolPolicy,
    chat::ConversationConfig,
    compaction::CompactionSettings,
    simulation::{Scenario, SequentialIds},
};
use playtime_harness::Session;
use playtime_local::{PrivateDirectory, Token};
use std::{
    fs,
    os::unix::fs::symlink,
    path::{Path, PathBuf},
};

struct Root(PathBuf);
impl Root {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "playtime-picker-{}",
            Token::generate().unwrap().expose()
        ));
        PrivateDirectory::open(&path, true).unwrap();
        Self(path)
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn config() -> ConversationConfig {
    let scenario: Scenario = serde_json::from_str(include_str!(
        "../../playtime-engine/tests/fixtures/two-session.json"
    ))
    .unwrap();
    let mut parameters = scenario.parameters.common();
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

fn completed(path: &Path, config: ConversationConfig) -> Session {
    let mut turn = Session::create(path, config)
        .unwrap()
        .begin("A saved conversation.".into(), SequentialIds, 0)
        .unwrap();
    for (i, outcome) in [
        Outcome::WorkspaceStarted,
        Outcome::ModelCompleted {
            response: ModelResponse {
                message: serde_json::json!({"role":"assistant","content":"Recorded."}),
                done: true,
                generated_tokens: Some(10),
                prompt_tokens: None,
            },
            finish_reason: "stop".into(),
        },
        Outcome::Cleaned(vec![]),
    ]
    .into_iter()
    .enumerate()
    {
        let effect_id = turn.pending().unwrap().id;
        turn.advance(i as u64 * 2, Input::Dispatched { effect_id })
            .unwrap();
        turn.advance(i as u64 * 2 + 1, Input::Complete { effect_id, outcome })
            .unwrap();
    }
    turn.finish().unwrap()
}

#[test]
fn hides_empty_sessions_without_deleting_evidence_and_preserves_nonempty_states() {
    let root = Root::new();
    let expected = config();
    let empty_path = root.0.join("empty");
    drop(Session::create(&empty_path, expected.clone()).unwrap());
    let empty = PrivateDirectory::open(&empty_path, false).unwrap();
    empty
        .write_new("diagnostic.log", b"exited before the first message")
        .unwrap();
    let empty_before: Vec<_> = empty
        .names()
        .unwrap()
        .into_iter()
        .map(|name| {
            let data = empty.read(&name, 1024 * 1024).unwrap();
            (name, data)
        })
        .collect();
    drop(completed(&root.0.join("ready"), expected.clone()));
    let held = completed(&root.0.join("busy"), expected.clone());
    let interrupted = root.0.join("interrupted");
    drop(
        Session::create(&interrupted, expected.clone())
            .unwrap()
            .begin("Inspect\n\x1b[31mthis".into(), SequentialIds, 0)
            .unwrap(),
    );
    let head_before = fs::read(interrupted.join("head.json")).unwrap();
    let mut changed = expected.clone();
    changed.instructions.push_str(" Extra.");
    drop(completed(&root.0.join("changed"), changed));
    let mut other = expected.clone();
    other.model = Sha256Digest::new("3".repeat(64)).unwrap();
    drop(completed(&root.0.join("other-model"), other));
    let mut other = expected.clone();
    other.workspace = Sha256Digest::new("4".repeat(64)).unwrap();
    drop(completed(&root.0.join("other-workspace"), other));
    symlink(root.0.join("ready"), root.0.join("linked")).unwrap();
    let entries = catalog(&root.0, &expected).unwrap();
    assert_eq!(entries.len(), 4);
    assert!(!entries.iter().any(|entry| entry.path == empty_path));
    assert_eq!(
        empty.names().unwrap(),
        empty_before
            .iter()
            .map(|(name, _)| name.clone())
            .collect::<Vec<_>>()
    );
    for (name, data) in empty_before {
        assert_eq!(empty.read(&name, 1024 * 1024).unwrap(), data);
    }
    for (name, state) in [
        ("ready", State::Ready),
        ("busy", State::InUse),
        ("changed", State::SettingsDiffer),
        ("interrupted", State::Interrupted),
    ] {
        let entry = entries.iter().find(|e| e.path.ends_with(name)).unwrap();
        assert_eq!(entry.state, state);
        if name == "interrupted" {
            assert_eq!(entry.preview, "Inspect\\n\\u{1b}[31mthis");
        }
    }
    assert_eq!(
        fs::read(interrupted.join("head.json")).unwrap(),
        head_before
    );
    let ready = entries
        .iter()
        .position(|e| e.state == State::Ready)
        .unwrap()
        + 1;
    let busy = entries
        .iter()
        .position(|e| e.state == State::InUse)
        .unwrap()
        + 1;
    let input = format!("{busy}\n{ready}\n");
    let mut output = Vec::new();
    let chosen = select(&entries, &mut input.as_bytes(), &mut output).unwrap();
    assert_eq!(chosen.session.unwrap(), root.0.join("ready"));
    assert!(
        String::from_utf8(output)
            .unwrap()
            .contains("Choose a resumable")
    );
    drop(held);
}

#[test]
fn new_cancel_and_eof_do_not_choose_an_existing_session() {
    for (input, cancelled) in [("\n", false), ("0\n", false), ("q\n", true), ("", true)] {
        let choice = select(&[], &mut input.as_bytes(), &mut Vec::new()).unwrap();
        assert_eq!(choice.cancelled, cancelled);
        assert!(choice.session.is_none());
    }
}

#[test]
fn corrupt_checkpoint_is_not_resumable() {
    let root = Root::new();
    let path = root.0.join("corrupt");
    let config = config();
    drop(Session::create(&path, config.clone()).unwrap());
    let directory = PrivateDirectory::open(&path, false).unwrap();
    let mut head: serde_json::Value =
        serde_json::from_slice(&directory.read("head.json", 4096).unwrap()).unwrap();
    head["checkpoint"] = serde_json::json!("missing-checkpoint");
    directory
        .replace("head.json", &serde_json::to_vec(&head).unwrap())
        .unwrap();
    let entries = catalog(&root.0, &config).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].state, State::Unavailable);
}
