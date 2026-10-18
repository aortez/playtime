#[path = "support/generated.rs"]
mod generated;
use egui_kittest::Harness;
use playtime_ui::{transcript::*, ui::Workbench};
#[test]
fn hundred_thousand_records_use_bounded_cache_and_virtualized_layout() {
    let mut index = Transcript::default();
    let mut bytes = 0;
    for n in 0..100_000 {
        let event = generated::event(n);
        bytes += serde_json::to_vec(&event).unwrap().len() + 1;
        index.apply(event, None).unwrap();
    }
    assert!(bytes <= 64 << 20, "generator used {bytes} bytes");
    assert_eq!(index.metrics.records, 100_000);
    assert!(index.metrics.rows >= 10_000);
    assert!(index.metrics.payload_high_water <= PAYLOAD_BYTES);
    assert!(index.metrics.index_high_water <= INDEX_BYTES);
    assert!(!index.contains(1), "cold payloads must be evicted");
    let mut app = Workbench::default();
    app.model.launcher = false;
    app.model.following = false;
    app.model.window = index.window(0, 64);
    let mut h = Harness::builder()
        .with_size([900.0, 600.0])
        .build_ui_state(|ui, app: &mut Workbench| app.draw(ui), app);
    h.run();
    assert!(h.state().metrics.laid_out_rows < 12);
    assert!(h.state().metrics.text_bytes <= TEXT_BYTES);
    let mut event = generated::event(0);
    index.apply(event.clone(), None).unwrap();
    assert!(index.raw(1, 0).is_some(), "revisit rehydrates exact source");
    event.data["unknown"] = serde_json::json!(true);
    assert!(index.apply(event, None).is_err());
    eprintln!(
        "generated_bytes={bytes}, metrics={:?}, frame={:?}",
        index.metrics,
        h.state().metrics
    );
}
