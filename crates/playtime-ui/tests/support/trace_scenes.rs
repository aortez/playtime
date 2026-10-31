//! Explicitly synthetic reading fixtures; no commands are executed.
use playtime_protocol::events::EventEnvelope;
use playtime_ui::transcript::{Transcript, View, Window};
use serde_json::json;
pub fn scene(name: &str) -> Window {
    let mut t = Transcript::default();
    for line in include_str!(
        "../../../playtime-protocol/tests/fixtures/controller-observations/completed.jsonl"
    )
    .lines()
    {
        let mut e: EventEnvelope = serde_json::from_str(line).unwrap();
        if e.kind != "controller_observation" {
            continue;
        }
        let seq = e.sequence.get();
        if name.contains("streaming") {
            if !(7..=9).contains(&seq) {
                continue;
            }
            if seq == 8 {
                e.data["data"]["value"]["chunk"]["message"] = json!({"thinking":"I will inspect the existing files before deciding what to change. "});
            }
            if seq == 9 {
                e.data["data"]["value"]["chunk"]["message"] = json!({"thinking":"The next step is to check whether the saved probe is present."});
            }
        } else {
            if !(7..=15).contains(&seq) && !(19..=25).contains(&seq) {
                continue;
            }
            let data = &mut e.data["data"]["value"];
            if seq == 12 {
                data["response"]["message"] = json!({"role":"assistant", "thinking":"Check the saved result before continuing.", "content":"", "tool_calls":[{"function":{"name":"read_file","arguments":{"path":"/work/result.txt"}}}]});
                if name.contains("long") {
                    data["response"]["message"]["thinking"] = json!("I will compare the observed output with the recorded inputs. This is recorded model text, not a verified conclusion.\n".repeat(120));
                }
            }
            if seq == 13 {
                *data = json!({"name":"read_file","arguments":{"path":"/work/result.txt"}});
            }
            if seq == 14 {
                data["name"] = json!("read_file");
            }
            if seq == 15 {
                data["name"] = json!("read_file");
                data["result"] = json!({"error":"file not found"});
            }
            if seq == 19 {
                data["handoff"] = json!(true);
            }
            if seq == 22 {
                data["response"]["message"] = json!({"role":"assistant","content":"","tool_calls":[{"function":{"name":"end_session","arguments":{"memento":"The result file was missing. Verify the probe next session.","finish_run":false}}}]});
            }
            if seq == 23 {
                data["arguments"] = json!({"memento":"The result file was missing. Verify the probe next session.","finish_run":false});
            }
            // Leave the submitted handoff without a result: publication is unknown.
            if seq == 25 {
                continue;
            }
        }
        t.apply(e, None).unwrap();
    }
    let mut window = t.window(View::Readable, 0, 64);
    window.jumps = t.jumps(0, true);
    window.calls.complete = true;
    window
}
