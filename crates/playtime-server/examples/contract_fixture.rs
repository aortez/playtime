//! Print synthetic contract examples for deliberate golden review.
use playtime_protocol::api::*;
use playtime_server::{application::Application, fixture::Fixture};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = Fixture::new(ServiceId::new("contract-fixture-service")?);
    let request = LaunchRequest {
        target: None,
        recipe_id: RecipeId::new("fixture-example")?,
        mode: playtime_core::RunMode::Simulation,
        preview_revision: PreviewRevision::new("fixture-v1")?,
    };
    let launch = fixture
        .admit(
            IdempotencyKey::new("contract-fixture-launch")?,
            request.clone(),
        )
        .await?;
    let empty = launch.run.event_anchors.clone().unwrap();
    fixture.reveal(
        &launch.run_id,
        4,
        Some(StreamEndStatus::CommittedCompletion),
    );
    let completed = fixture.run(launch.run_id.clone()).await?;
    let history = fixture
        .history(launch.run_id.clone(), None, false, 1, 1 << 20)
        .await?;
    let cursor = EventCursor::new(history.frontier.clone(), 1)?;
    let page = EventPage {
        events: history.events,
        frontier: history.frontier,
        has_more: true,
        next_cursor: Some(cursor.encode()?),
        resume_cursor: cursor.encode()?,
    };
    let artifacts = fixture
        .artifacts(launch.run_id.clone(), PageQuery::default())
        .await?;
    let checkpoints = fixture
        .checkpoints(launch.run_id.clone(), PageQuery::default())
        .await?;
    let value = serde_json::json!({"launch_request":request,"launch_response":launch,"empty_anchors":empty,
        "completed_run":completed,"event_page":page,"cursor_json":cursor,"cursor_encoded":cursor.encode()?,
        "checkpoints":checkpoints,"artifacts":artifacts});
    println!("{}", serde_json::to_string_pretty(&value)?);
    Ok(())
}
