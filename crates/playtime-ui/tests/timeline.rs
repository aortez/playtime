//! The timeline lists each step once in Compact and Readable, with one-line
//! markers for session starts, checkpoints and the run's end (#92).
use egui_kittest::{Harness, kittest::Queryable};
use playtime_protocol::{api::Checkpoint, events::EventEnvelope};
use playtime_ui::{
    backend::checkpoint_files,
    model::{Action, Disclosure},
    transcript::{Row, Transcript, View, Window},
    ui::Workbench,
};
use serde_json::json;

/// Archive records that repeat what the observations already show.
const BOOKKEEPING: [&str; 9] = [
    "run_started",
    "phase_changed",
    "session_started",
    "effect_intent",
    "request_finished",
    "command_finished",
    "session_ended",
    "checkpoint_published",
    "run_finished",
];

fn events(jsonl: &str) -> Vec<EventEnvelope> {
    jsonl
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}
/// The two-session archive plus one dispatch intent, so every kind is present.
fn archive() -> Vec<EventEnvelope> {
    let mut archive = events(include_str!(
        "../../playtime-protocol/tests/fixtures/two-session/events.jsonl"
    ));
    let mut intent = events(include_str!(
        "../../playtime-protocol/tests/fixtures/recovery/events.jsonl"
    ))
    .into_iter()
    .find(|event| event.kind == "effect_intent")
    .unwrap();
    intent.run_id = archive[0].run_id.clone();
    intent.sequence = std::num::NonZeroU64::new(17).unwrap();
    archive.push(intent);
    archive
}

#[test]
fn readable_shows_markers_and_full_keeps_every_record() {
    let archive = archive();
    let mut transcript = Transcript::default();
    // Late history arrives after live records.
    for event in archive.iter().rev() {
        transcript.apply(event.clone(), None).unwrap();
    }
    let full = transcript.window(View::Full, 0, 64);
    assert_eq!(full.total, archive.len());
    for kind in BOOKKEEPING {
        assert!(
            full.rows.iter().any(|row| row.title == kind && !row.marker),
            "Full lost the {kind} card"
        );
    }
    let readable = transcript.window(View::Readable, 0, 64);
    let titles = readable
        .rows
        .iter()
        .map(|row| row.title.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        titles,
        [
            "s1 · Session 1 started without a memento",
            "s1 · Checkpoint saved with a memento",
            "s2 · Session 2 started with a memento (27 bytes)",
            "s2 · Checkpoint saved with a memento",
            "Run finished: agent finished · cleanup complete · 110 tokens charged",
        ]
    );
    assert!(
        readable
            .rows
            .iter()
            .all(|row| row.marker && row.excerpt.is_empty() && row.status.is_empty())
    );
    assert_eq!(
        readable
            .rows
            .iter()
            .map(|row| row.checkpoint.as_deref())
            .collect::<Vec<_>>(),
        [None, Some("s1"), None, Some("s2"), None]
    );
    // Inspect source works for every row in both views.
    for row in readable.rows.iter().chain(&full.rows) {
        assert!(transcript.sources(row.id).contains(&row.first_sequence));
        assert!(transcript.raw(row.first_sequence, 0).is_some());
    }
}

#[test]
fn readable_keeps_every_observation_and_drops_its_archive_repeats() {
    let records = events(include_str!(
        "../../playtime-protocol/tests/fixtures/controller-observations/completed.jsonl"
    ));
    let repeats = records
        .iter()
        .filter(|event| event.kind != "controller_observation")
        .count();
    let mut transcript = Transcript::default();
    for event in records {
        transcript.apply(event, None).unwrap();
    }
    let full = transcript.window(View::Full, 0, 64);
    let readable = transcript.window(View::Readable, 0, 64);
    assert_eq!(repeats, 12);
    assert_eq!(readable.total, full.total - repeats);
    assert!(
        readable
            .rows
            .iter()
            .all(|row| !BOOKKEEPING.contains(&row.title.as_str()))
    );
    // Each phase change appears once, as its observation.
    let phases = |window: &Window| {
        window
            .rows
            .iter()
            .filter(|row| row.title.ends_with("phase_changed"))
            .count()
    };
    assert_eq!((phases(&readable), phases(&full)), (8, 16));
}

#[test]
fn a_record_left_out_of_readable_maps_to_the_next_row_shown() {
    let mut transcript = Transcript::default();
    for event in archive() {
        transcript.apply(event, None).unwrap();
    }
    // In Full every archive record is its own row, in sequence order.
    let full = transcript.window(View::Full, 0, 64);
    let id = |sequence: u64| {
        full.rows
            .iter()
            .find(|row| row.first_sequence == sequence)
            .unwrap()
            .id
    };
    // Readable lists sequences 3, 8, 9, 13 and 16.
    for (sequence, readable) in [(1, 0), (2, 0), (3, 0), (4, 1), (8, 1), (14, 4), (17, 4)] {
        assert_eq!(
            transcript.position(View::Readable, id(sequence)),
            Some(readable),
            "sequence {sequence}"
        );
        assert_eq!(
            transcript.position(View::Full, id(sequence)),
            Some(sequence as usize - 1)
        );
    }
}

#[test]
fn checkpoint_markers_name_the_files_and_images_each_session_saved() {
    let contract: serde_json::Value = serde_json::from_str(include_str!(
        "../../playtime-server/tests/fixtures/http-contract.json"
    ))
    .unwrap();
    let template = &contract["checkpoints"]["items"][0];
    let checkpoint = |session: &str, sequence: u64, files: &[(&str, &str, Option<char>)]| {
        let mut value = template.clone();
        value["checkpoint"] = json!({"session_id": session, "published_sequence": sequence});
        value["files"] = files
            .iter()
            .map(|(name, kind, digest)| {
                let mut file = template["files"][if digest.is_some() { 0 } else { 1 }].clone();
                file["name"] = json!(name);
                file["kind"] = json!(kind);
                if let Some(digest) = digest {
                    file["content"]["content"]["sha256"] = json!(digest.to_string().repeat(64));
                }
                file
            })
            .collect();
        serde_json::from_value::<Checkpoint>(value).unwrap()
    };
    let mut added = vec![
        ("seeds/a.txt", "0", Some('a')),
        ("art.png", "0", Some('c')),
        ("notes.md", "regular", Some('d')),
        ("work", "5", None),
    ];
    for name in ["f1.txt", "f2.txt", "f3.txt", "f4.txt", "f5.txt", "f6.txt"] {
        added.push((name, "0", Some('e')));
    }
    let checkpoints = [
        checkpoint(
            "s2",
            20,
            &[
                ("seeds/a.txt", "0", Some('a')),
                ("art.png", "0", Some('c')),
                ("notes.md", "regular", Some('d')),
                ("unretained.txt", "0", None),
                ("work", "5", None),
            ],
        ),
        checkpoint(
            "s1",
            10,
            &[
                ("seeds/a.txt", "0", Some('a')),
                ("art.png", "0", Some('b')),
                ("work", "5", None),
            ],
        ),
        checkpoint("s3", 30, &added),
    ];
    let files = checkpoint_files(&checkpoints, true);
    assert_eq!(
        files["s1"],
        "2 files, 1 image · new or changed: art.png, seeds/a.txt"
    );
    assert_eq!(
        files["s2"],
        "4 files, 1 image · new or changed: art.png, notes.md, unretained.txt"
    );
    assert_eq!(
        files["s3"],
        "9 files, 1 image · new or changed: f1.txt, f2.txt, f3.txt, f4.txt and 2 more"
    );
    // Without the whole list, the earliest checkpoint has nothing to compare.
    let partial = checkpoint_files(&checkpoints, false);
    assert_eq!(partial["s1"], "2 files, 1 image");
    assert_eq!(partial["s2"], files["s2"]);
}

fn rows(ids: impl Iterator<Item = usize>, marker: bool) -> Vec<Row> {
    ids.map(|id| Row {
        id,
        first_sequence: id as u64 + 1,
        title: format!("s1 · row {id}"),
        excerpt: "Final response recorded".into(),
        status: "Recorded".into(),
        sources: format!("1 source records · first {} · last {}", id + 1, id + 1),
        marker,
        checkpoint: marker.then(|| "s1".into()),
        ..Default::default()
    })
    .collect()
}

#[test]
fn a_marker_is_one_line_with_what_its_checkpoint_saved() {
    let mut app = Workbench::default();
    app.model.launcher = false;
    app.model.following = false;
    app.model.window = Window {
        total: 1,
        rows: rows(0..1, true),
        ..Default::default()
    };
    app.model.checkpoint_files.insert(
        "s1".into(),
        "2 files, 1 image · new or changed: art.png".into(),
    );
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    h.get_by_label("s1 · row 0");
    h.get_by_label("2 files, 1 image · new or changed: art.png");
    h.get_by_label("Inspect source 1");
    assert!(h.query_by_label("Recorded").is_none());
    assert!(
        h.query_by_label("1 source records · first 1 · last 1")
            .is_none()
    );
}

#[test]
fn switching_to_full_keeps_the_top_row_in_place() {
    let mut app = Workbench::default();
    app.model.launcher = false;
    app.model.following = false;
    // Readable leaves out every other record.
    app.model.window = Window {
        total: 40,
        rows: rows((0..40).map(|i| i * 2), false),
        ..Default::default()
    };
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    h.hover_at(egui::pos2(670.0, 600.0));
    h.event(egui::Event::MouseWheel {
        unit: egui::MouseWheelUnit::Point,
        delta: egui::vec2(0.0, -2400.0),
        phase: egui::TouchPhase::Move,
        modifiers: egui::Modifiers::NONE,
    });
    h.run_steps(8);
    h.get_by_label("Full").click();
    h.run();
    let request = h
        .state()
        .model
        .commands
        .iter()
        .rev()
        .find_map(|command| match command.action {
            Action::Window(request) => Some(request),
            _ => None,
        })
        .unwrap();
    assert_eq!(h.state().model.preferences.disclosure, Disclosure::Full);
    assert_eq!(request.view, View::Full);
    let anchor = request
        .anchor
        .expect("the top row travels with the request");
    assert!(anchor > 10, "the wheel did not scroll the timeline");
    // The backend answers with Full's rows, where every record is listed.
    let start = anchor - 2;
    h.state_mut().model.window = Window {
        start,
        total: 80,
        rows: rows(start..80.min(start + 64), false),
        anchor: Some((anchor, anchor)),
        ..Default::default()
    };
    h.run();
    let top = (start..80)
        .filter_map(|id| {
            h.query_by_label(&format!("Inspect source {}", id + 1))
                .map(|node| node.rect().top())
        })
        .fold(f32::INFINITY, f32::min);
    let anchored = h
        .get_by_label(&format!("Inspect source {}", anchor + 1))
        .rect();
    assert!(
        (anchored.top() - top).abs() < 1.0,
        "row {anchor} is not the top row after switching views"
    );
}
