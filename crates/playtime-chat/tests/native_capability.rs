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
