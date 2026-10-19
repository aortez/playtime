//! Reproducible, hardware-dependent release measurement; never a timing CI gate.
#[path = "../tests/support/generated.rs"]
mod generated;
use egui_kittest::Harness;
use playtime_ui::{
    model::{Action, Disclosure},
    transcript::{Transcript, View},
    ui::Workbench,
};
use std::time::{Duration, Instant};
fn rss() -> usize {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find(|l| l.starts_with("VmHWM:"))
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}
fn main() {
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .unwrap()
        .lines()
        .find(|l| l.starts_with("model name"))
        .unwrap()
        .to_owned();
    let mut index = Transcript::default();
    let mut serialized = 0;
    for n in 0..100_000 {
        let event = generated::event(n);
        serialized += serde_json::to_vec(&event).unwrap().len() + 1;
        index.apply(event, None).unwrap();
    }
    let mut app = Workbench::default();
    app.model.launcher = false;
    app.model.following = false;
    app.model.window = index.window(View::Readable, 0, 64);
    let mut h = Harness::builder()
        .with_size([1440.0, 900.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    let start = Instant::now();
    let mut frames = vec![];
    let mut added = 0;
    let mut max_rows = 0;
    let mut max_text = 0;
    while start.elapsed() < Duration::from_secs(60) {
        let target = (start.elapsed().as_secs_f64() * 100.0) as usize;
        while added < target {
            index
                .apply(generated::event(100_000 + added), None)
                .unwrap();
            added += 1;
        }
        let seconds = start.elapsed().as_secs();
        h.state_mut().model.preferences.disclosure = match seconds % 3 {
            0 => Disclosure::Compact,
            1 => Disclosure::Readable,
            _ => Disclosure::Full,
        };
        // Explicitly switch visible source windows and clear the previous run's
        // transient selection; parsing/index construction is timed separately.
        let position = ((seconds * 173) as usize) % (index.metrics.rows - 64);
        h.state_mut().model.window = index.window(View::Readable, position, 64);
        h.state_mut().model.window.start = 0; // Small viewport represents the selected page.
        h.state_mut().model.window.total = 64;
        if seconds.is_multiple_of(5) {
            h.state_mut().model.selected_row = None;
            h.state_mut().model.source = None;
        }
        h.state_mut()
            .model
            .commands
            .retain(|c| !matches!(c.action, Action::Window(..) | Action::Preferences(_)));
        let frame_start = Instant::now();
        h.step();
        frames.push(frame_start.elapsed().as_secs_f64() * 1000.0);
        max_rows = max_rows.max(h.state().metrics.laid_out_rows);
        max_text = max_text.max(h.state().metrics.text_bytes);
        std::thread::sleep(Duration::from_millis(10));
    }
    frames.sort_by(f64::total_cmp);
    println!(
        "cpu={cpu}\nseconds={:.3}\nframes={}\nlive_records={}\nserialized_initial_bytes={}\np50_ms={:.3}\np95_ms={:.3}\np99_ms={:.3}\npeak_rss_kib={}\nmax_laid_out_rows={}\nmax_text_bytes={}\nmetrics={:?}",
        start.elapsed().as_secs_f64(),
        frames.len(),
        added,
        serialized,
        frames[frames.len() / 2],
        frames[frames.len() * 95 / 100],
        frames[frames.len() * 99 / 100],
        rss(),
        max_rows,
        max_text,
        index.metrics
    );
    assert!(
        frames[frames.len() * 95 / 100] < 33.0,
        "Investigate missed frame target before accepting this measurement"
    );
}
