use playtime_client::{Client, Observation, RetryPolicy};
use playtime_core::RunMode;
use playtime_protocol::api::{LaunchRequest, PageQuery, RecipeId};
use std::path::PathBuf;
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    if args.next().as_deref() != Some("--connection-file") {
        return Err("usage: api_walkthrough --connection-file <path>".into());
    }
    let connection = PathBuf::from(args.next().ok_or("connection path required")?);
    if args.next().is_some() {
        return Err("unexpected argument".into());
    }
    let client = Client::from_connection(&connection).await?;
    println!(
        "Backend: {:?}: {}",
        client.capabilities().backend.kind,
        client.capabilities().backend.description
    );
    let recipes = client.recipes(&PageQuery::default()).await?;
    println!("{} scripted recipes", recipes.items.len());
    let preview = client
        .preview(&RecipeId::new("fixture-example")?, RunMode::Simulation)
        .await?;
    let pending = client.prepare_launch(
        LaunchRequest {
            recipe_id: preview.recipe_id,
            mode: preview.mode,
            preview_revision: preview.preview_revision,
        },
        None,
    )?;
    // Retain the complete service/key/request BEFORE sending. This walkthrough
    // writes a receipt outside any archive; #21 owns the user-facing receipt UX.
    let receipt = connection
        .parent()
        .unwrap()
        .join(format!("walkthrough-{}.json", pending.key));
    use tokio::io::AsyncWriteExt;
    let mut file = tokio::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&receipt)
        .await?;
    file.write_all(&serde_json::to_vec_pretty(&pending)?)
        .await?;
    file.sync_all().await?;
    let launch = client.launch(&pending).await?;
    let lookup = client.lookup(&pending).await?;
    assert_eq!(launch.run_id, lookup.run_id);
    let replay = client.launch(&pending).await?;
    assert!(replay.replayed);
    let view = client.run(&launch.run_id).await?;
    let anchors = view.event_anchors.ok_or("fixture evidence not ready")?;
    let mut history = Some(anchors.history_cursor);
    while let Some(cursor) = history {
        let page = client
            .history(
                &launch.run_id,
                &PageQuery {
                    cursor: Some(cursor),
                    limit: Some(1),
                },
            )
            .await?;
        println!(
            "History: {} records through {}",
            page.events.len(),
            page.frontier.prefix.sequence
        );
        history = page.next_cursor;
    }
    let cancel = CancellationToken::new();
    let mut first = client
        .stream(&launch.run_id, Some(&anchors.live_cursor), &cancel)
        .await?;
    let saved = first.acknowledged_cursor().map(str::to_owned);
    if let Observation::Record(record) = first.next(&cancel).await? {
        println!(
            "Delivered {} before reconnect; deliberately unacknowledged",
            record.event.sequence
        );
    }
    drop(first);
    let mut follow = client.follow(&launch.run_id, saved, RetryPolicy::default());
    loop {
        match follow.next(&cancel).await? {
            Observation::Record(record) => {
                println!("Record {}: {}", record.event.sequence, record.event.kind);
                follow.acknowledge(&record)?;
            }
            Observation::End(end) => {
                println!("Reconciled stream end: {:?}", end.status);
                break;
            }
            Observation::Disconnected => {
                return Err("stream disconnected without reconciled end".into());
            }
        }
    }
    let checkpoint = client
        .checkpoints(&launch.run_id, &PageQuery::default())
        .await?;
    println!("{} published fixture checkpoints", checkpoint.items.len());
    let artifacts = client
        .artifacts(&launch.run_id, &PageQuery::default())
        .await?;
    let artifact = artifacts
        .items
        .iter()
        .find(|a| a.kind == "file_version")
        .ok_or("missing fixture image")?;
    let destination = connection
        .parent()
        .unwrap()
        .join(format!("walkthrough-{}.png", pending.key));
    let bytes = client
        .download_to_path(&launch.run_id, artifact, &destination, &cancel)
        .await?;
    println!("Verified {bytes} bytes: {}", destination.display());
    Ok(())
}
