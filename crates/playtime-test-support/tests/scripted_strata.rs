//! `scripts/chat-sandbox.py` runs the chat console against `scripted-strata`.
//! The real Strata client must verify that endpoint against the lock it writes,
//! and queued replies must arrive in order, with call IDs that differ across
//! responses as a conversation needs.
#[path = "../../playtime-strata/tests/support/mod.rs"]
mod strata;

use playtime_strata::{Client, Endpoint, ModelLock};
use playtime_test_support::TestRoot;
use serde_json::{Value, json};
use std::{
    fs,
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::{sync::mpsc, time::Instant};
use tokio_util::sync::CancellationToken;

struct Running(Child);
impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn the_sandbox_endpoint_verifies_and_answers_from_its_queue() {
    let root = TestRoot::new().unwrap();
    let mut server = Running(
        Command::new(env!("CARGO_BIN_EXE_scripted-strata"))
            .arg(&root.0)
            .args(["--pace-ms", "0"])
            .stdout(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !root.join("endpoint").exists() {
        assert!(server.0.try_wait().unwrap().is_none(), "the server exited");
        assert!(std::time::Instant::now() < deadline, "no endpoint");
        std::thread::sleep(Duration::from_millis(20));
    }
    let endpoint = Endpoint::parse(&fs::read_to_string(root.join("endpoint")).unwrap()).unwrap();
    let lock: ModelLock =
        serde_json::from_slice(&fs::read(root.join("model.lock.json")).unwrap()).unwrap();
    let mut client = Client::new(endpoint, lock, Default::default()).unwrap();
    let cancel = CancellationToken::new();
    client
        .verify(Instant::now() + Duration::from_secs(10), &cancel)
        .await
        .unwrap();

    let call = json!({"calls":[{"name":"read_file","arguments":{"path":"notes.txt"}}]});
    for name in ["0001.json", "0002.json"] {
        fs::write(root.join("replies").join(name), call.to_string()).unwrap();
    }
    let mut responses = Vec::new();
    for _ in 0..3 {
        let (events, _receiver) = mpsc::channel(1024);
        let deadline = Instant::now() + Duration::from_secs(10);
        let result = client
            .complete(&strata::request(), deadline, &cancel, events)
            .await;
        responses.push(result.completion.unwrap().response);
    }

    let id = |n: usize| responses[n].message["tool_calls"][0]["id"].clone();
    assert_eq!(
        responses[0].message["tool_calls"][0]["function"]["name"],
        "read_file"
    );
    assert_ne!(id(0), id(1));
    assert!(id(0).as_str().is_some_and(|id| !id.is_empty()));
    let answer = responses[2].message["content"].as_str().unwrap();
    assert!(answer.ends_with("You said: Inspect the file."), "{answer}");

    // Input usage counts four bytes a token of the request as sent.
    let body: Value =
        serde_json::from_slice(&fs::read(root.join("requests/0001.json")).unwrap()).unwrap();
    let sent = serde_json::to_vec(&body).unwrap().len() as u64;
    assert_eq!(responses[0].prompt_tokens, Some(sent.div_ceil(4)));

    let replies: Vec<String> = fs::read_to_string(root.join("requests.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["reply"].to_string())
        .collect();
    assert_eq!(replies, ["\"0001.json\"", "\"0002.json\"", "\"default\""]);
    assert!(root.join("replies/0002.json.used").exists());
}
