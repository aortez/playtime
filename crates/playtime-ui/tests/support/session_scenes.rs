//! Focused, explicitly synthetic boundary scenes. No commands execute.
use playtime_protocol::events::EventEnvelope;
use playtime_ui::{
    model::TraceView,
    transcript::{Section, Transcript, View},
    ui::Workbench,
};
use serde_json::json;
pub fn prepare(app: &mut Workbench, interrupted: bool) {
    let records: Vec<EventEnvelope> = include_str!(
        "../../../playtime-protocol/tests/fixtures/controller-observations/completed.jsonl"
    )
    .lines()
    .map(|l| serde_json::from_str(l).unwrap())
    .collect();
    let mut transcript = Transcript::default();
    let mut received = records[3].clone();
    received.data["session_id"] = json!("s2");
    received.data["data"]["value"]["ordinal"] = json!(2);
    received.data["data"]["value"]["memory"] =
        json!({"status":"loaded","content":"Probe survived. Verify result.txt before continuing."});
    transcript.apply(received, None).unwrap();
    if interrupted {
        for mut event in records
            .into_iter()
            .filter(|e| [19, 22, 23, 24, 58, 59].contains(&e.sequence.get()))
        {
            if event.data["session_id"].is_string() {
                event.data["session_id"] = json!("s2");
            }
            if event.sequence.get() == 19 {
                event.data["data"]["value"]["handoff"] = json!(true);
            }
            if event.sequence.get() == 22 {
                event.data["data"]["value"]["response"]["message"] = json!({"role":"assistant","content":"","tool_calls":[{"function":{"name":"end_session","arguments":{"memento":"The result is complete. Continue with the chart.","finish_run":false}}}]});
            }
            if event.sequence.get() == 59 {
                event.data["data"]["value"]["reason"] = json!("interrupted");
            }
            if event.sequence.get() == 23 {
                event.data["data"]["value"]["arguments"] = json!({"memento":"The result is complete. Continue with the chart.","finish_run":false});
            }
            transcript.apply(event, None).unwrap();
        }
    } else {
        let mut checkpoint: EventEnvelope = serde_json::from_str(
            include_str!("../../../playtime-protocol/tests/fixtures/two-session/events.jsonl")
                .lines()
                .nth(7)
                .unwrap(),
        )
        .unwrap();
        checkpoint.run_id = records[0].run_id.clone();
        checkpoint.sequence = 30.try_into().unwrap();
        checkpoint.data["checkpoint"]["session_id"] = json!("s2");
        transcript.apply(checkpoint, None).unwrap();
        app.model.trace.insert(
            30,
            TraceView {
                sections: vec![
                    Section {
                        label: "Memory received".into(),
                        text: "Probe survived. Verify result.txt before continuing.".into(),
                    },
                    Section {
                        label: "Published memento".into(),
                        text: "Result verified and saved. Next: compare the chart.".into(),
                    },
                ],
                files: "2 saved files · 1 image: result.txt, chart.png".into(),
                notice: "Saved checkpoint versions · model claims are not verified.".into(),
            },
        );
    }
    app.model.window = transcript.window(View::Readable, 0, 64);
    app.model.window.sessions = transcript.sessions(0, true);
    app.model.window.jumps = transcript.jumps(0, true);
    app.model.window.calls.complete = true;
    app.model.run = None;
    app.model.history = "Synthetic session-boundary fixture · no commands executed".into();
    app.model.notice = if interrupted {
        "Interrupted handoff · no new checkpoint was published.".into()
    } else {
        "Exact delivered memory and published checkpoint memory remain distinct.".into()
    };
}
