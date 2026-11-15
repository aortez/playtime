//! Fast subprocess acceptance: real shared-controller checkpoint, real CLI,
//! bounded local files, no model, Docker, network or desktop required.
use playtime_core::{RunMode, Sha256Digest};
use playtime_engine::{
    chat::ConversationConfig,
    compaction::CompactionSettings,
    simulation::{Scenario, SequentialIds},
    *,
};
use playtime_harness::{
    Session,
    artifacts::{Artifacts, blob_name},
};
use playtime_local::Token;
use serde_json::{Value, json};
use std::{
    fs,
    io::Cursor,
    os::unix::fs::{MetadataExt, symlink},
    path::{Path, PathBuf},
    process::{Command, Output},
};

struct Fixture {
    root: PathBuf,
    session: PathBuf,
    project: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "playtime-artifacts-{}",
            Token::generate().unwrap().expose()
        ));
        let session = root.join("session");
        let project = root.join("project");
        let scenario: Scenario = serde_json::from_str(include_str!(
            "../../playtime-engine/tests/fixtures/two-session.json"
        ))
        .unwrap();
        let mut parameters = scenario.parameters.common();
        parameters.limits.context_tokens = 16384.try_into().unwrap();
        parameters.limits.tokens_per_call = 512.try_into().unwrap();
        parameters.limits.generated_tokens = 10000.try_into().unwrap();
        parameters.limits.output_bytes = 2048.try_into().unwrap();
        let config = ConversationConfig {
            schema_version: 1,
            mode: RunMode::Simulation,
            tool_policy: ToolPolicy::Simulation,
            model: Sha256Digest::new("1".repeat(64)).unwrap(),
            workspace: Sha256Digest::new("2".repeat(64)).unwrap(),
            instructions: "Answer".into(),
            inventory: "Fixture".into(),
            parameters,
            compaction: CompactionSettings {
                allow_finish_run: false,
                summary_tokens: 256.try_into().unwrap(),
                ..Default::default()
            },
        };
        let mut turn = Session::create(&session, config)
            .unwrap()
            .begin("Check the screen".into(), SequentialIds, 0)
            .unwrap();
        for (now, outcome) in [
            Outcome::WorkspaceStarted,
            Outcome::ModelCompleted {
                response: ModelResponse {
                    message: json!({"role":"assistant","content":"Screen checked"}),
                    done: true,
                    generated_tokens: Some(3),
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
            turn.advance(now as u64 * 2, Input::Dispatched { effect_id })
                .unwrap();
            turn.advance(now as u64 * 2 + 1, Input::Complete { effect_id, outcome })
                .unwrap();
        }
        drop(turn.finish().unwrap());
        fs::create_dir(&project).unwrap();
        let mut png = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(4, 3)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        fs::write(project.join("screen test.png"), png.into_inner()).unwrap();
        fs::write(project.join("diagram.mmd"), "graph LR\n A-->B\n").unwrap();
        Self {
            root,
            session,
            project,
        }
    }
    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_playtime-artifacts"))
            .args(["--json", "--session"])
            .arg(&self.session)
            .arg("--workspace")
            .arg(&self.project)
            .args(args)
            .output()
            .unwrap()
    }
    fn success(&self, args: &[&str]) -> Value {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn retain(&self) -> String {
        self.success(&[
            "retain",
            "screen test.png",
            "--caption",
            "Screen test after build",
            "--source",
            "diagram.mmd",
        ])["result"]["artifact"]["id"]
            .as_str()
            .unwrap()
            .into()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn cli_retains_reopens_after_build_replacement_and_exports_verified_diagram() {
    let fixture = Fixture::new();
    let original = fs::read(fixture.project.join("screen test.png")).unwrap();
    let id = fixture.retain();
    fs::write(fixture.project.join("screen test.png"), b"new build output").unwrap();
    fs::remove_file(fixture.project.join("diagram.mmd")).unwrap();
    let listed = fixture.success(&["list"]);
    assert_eq!(listed["result"]["catalog"]["artifacts"][0]["id"], id);
    let fallback = Command::new(env!("CARGO_BIN_EXE_playtime-console"))
        .arg("--artifact-session")
        .arg(&fixture.session)
        .output()
        .unwrap();
    assert!(fallback.status.success());
    assert!(String::from_utf8_lossy(&fallback.stdout).contains("Screen test after build"));
    assert!(!String::from_utf8_lossy(&fallback.stdout).contains("Fixture complete"));
    let info = fixture.success(&["info", &id]);
    assert_eq!(info["result"]["verified"], true);
    assert_eq!(info["result"]["artifact"]["kind"], "diagram");
    let export = fixture.root.join("export");
    fixture.success(&["export", &id, "--output", export.to_str().unwrap()]);
    let record = Artifacts::open(&fixture.session)
        .unwrap()
        .info(&id)
        .unwrap();
    assert_eq!(
        fs::read(export.join(blob_name(&record, false).unwrap())).unwrap(),
        original
    );
    assert_eq!(
        fs::read_to_string(export.join(blob_name(&record, true).unwrap())).unwrap(),
        "graph LR\n A-->B\n"
    );
    assert_eq!(fs::metadata(&export).unwrap().mode() & 0o777, 0o700);
    assert_eq!(
        fs::metadata(export.join("artifact.json")).unwrap().mode() & 0o777,
        0o600
    );
    assert!(
        !fixture
            .command(&["export", &id, "--output", export.to_str().unwrap()])
            .status
            .success()
    );
}

#[test]
fn cli_refuses_path_escape_active_lease_corruption_and_unsupported_open() {
    let fixture = Fixture::new();
    symlink("screen test.png", fixture.project.join("link.png")).unwrap();
    symlink(&fixture.root, fixture.project.join("outside")).unwrap();
    rustix::fs::mkfifoat(
        rustix::fs::CWD,
        fixture.project.join("pipe"),
        rustix::fs::Mode::from_raw_mode(0o600),
    )
    .unwrap();
    for path in [
        "../session/config.json",
        "/etc/passwd",
        "link.png",
        "outside/session/config.json",
        "pipe",
    ] {
        assert!(
            !fixture
                .command(&["retain", path, "--caption", "No"])
                .status
                .success(),
            "{path}"
        );
    }
    let held = playtime_harness::artifacts::open_for_retention(&fixture.session).unwrap();
    assert!(
        !fixture
            .command(&["retain", "screen test.png", "--caption", "Busy"])
            .status
            .success()
    );
    drop(held);
    let file = fixture.success(&["retain", "diagram.mmd", "--caption", "Source only"]);
    let file_id = file["result"]["artifact"]["id"].as_str().unwrap();
    assert!(!fixture.command(&["open", file_id]).status.success());
    let id = fixture.retain();
    let store = Artifacts::open(&fixture.session).unwrap();
    let record = store.info(&id).unwrap();
    fs::write(
        fixture.session.join(blob_name(&record, false).unwrap()),
        b"corrupted",
    )
    .unwrap();
    assert!(!fixture.command(&["open", &id]).status.success());
    assert!(!fixture.command(&["info", &id]).status.success());
    let export = fixture.root.join("bad-export");
    assert!(
        !fixture
            .command(&["export", &id, "--output", export.to_str().unwrap()])
            .status
            .success()
    );
    assert!(!export.exists());
}

#[test]
fn console_commands_share_cli_parser_and_never_interpret_shell_syntax() {
    use playtime_chat::artifacts::{ArtifactCommand, parse_action};
    let ArtifactCommand::Retain { file, caption, source } = parse_action("/artifact retain 'screen test.png' --caption 'A screenshot; $(false)' --source diagram.mmd").unwrap() else { panic!("wrong action") };
    assert_eq!(file, Path::new("screen test.png"));
    assert_eq!(caption, "A screenshot; $(false)");
    assert_eq!(source.unwrap(), Path::new("diagram.mmd"));
    assert!(parse_action("/artifact retain 'unfinished").is_err());
    assert!(parse_action("/artifact delete anything").is_err());
    assert!(matches!(
        parse_action("/artifacts").unwrap(),
        ArtifactCommand::List
    ));
}
