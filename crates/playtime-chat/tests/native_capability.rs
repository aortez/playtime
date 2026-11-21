use serde_json::Value;
use std::process::Command;

#[test]
fn native_capability_is_explicit_and_needs_no_image_lock() {
    let output = Command::new(env!("CARGO_BIN_EXE_playtime-task"))
        .args(["--tool-backend", "native", "--describe-protocol"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["tools"], "project-native-v1");
    assert_eq!(value["tool_backends"]["docker"], "project-docker-v1");
    assert_eq!(value["tool_backends"]["native"], "project-native-v1");
    assert_eq!(value["roles"], serde_json::json!(["worker"]));
    assert_eq!(value["report"], "worker-report-v1");
    assert_eq!(
        value["native_helper_sha256"],
        playtime_chat::native_project::helper_sha256()
    );
    let output = Command::new(env!("CARGO_BIN_EXE_playtime-task"))
        .arg("--describe-protocol")
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["tools"], "project-docker-v1");
}

#[test]
fn readonly_capability_is_separate_from_worker_reports_and_never_resumes() {
    for role in ["setup", "supervisor", "reviewer"] {
        let output = Command::new(env!("CARGO_BIN_EXE_playtime-task"))
            .args(["--role", role, "--describe-protocol"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["protocol"], "playtime-readonly");
        assert_eq!(value["capability"], "playtime-readonly-v1");
        assert_eq!(value["report"], "structured-result-v1");
        assert_eq!(value["continuation"], "none");
        assert_eq!(value["transport"], "strata-responses-v1");
        assert_eq!(value["output_contract"], "json-schema-subset-v1");
        assert_eq!(value["tools"], "project-native-v1");
    }
    for extra in [
        vec![],
        vec!["--tool-backend", "docker"],
        vec!["--tool-backend", "native", "--worker-session", "ptw-saved"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_playtime-task"))
            .args(["--role", "reviewer", "--check-inputs"])
            .args(extra)
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("fresh invocations only"));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_playtime-task"))
        .args(["--output-contract", "/not/read", "--check-inputs"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("explicit read-only role"));
}
