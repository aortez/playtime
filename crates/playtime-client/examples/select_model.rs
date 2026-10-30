//! Save an explicit registered-model clone; never launches inference.
use playtime_client::Client;
use playtime_core::RunMode;
use playtime_protocol::api::*;
use std::{io::Write, os::unix::fs::OpenOptionsExt, path::Path};

fn retain(path: &Path, value: &impl serde::Serialize) -> Result<(), Box<dyn std::error::Error>> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(&serde_json::to_vec_pretty(value)?)?;
    file.sync_all()?;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 6 {
        return Err("usage: select_model CONNECTION SOURCE_RECIPE MODEL SETTINGS_JSON NEW_EVIDENCE_DIR EXPECTED_MODEL_DIGEST".into());
    }
    let settings: RecipeModelSettings = serde_json::from_slice(&std::fs::read(&args[3])?)?;
    if let Some(error) = settings.validation_error() {
        return Err(error.into());
    }
    let evidence = Path::new(&args[4]);
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(0o700).create(evidence)?;
    let client = Client::from_connection(Path::new(&args[0])).await?;
    let review = client
        .review_model(&ModelInspectRequest {
            name: args[2].clone(),
            expected_digest: Some(args[5].clone()),
        })
        .await?;
    retain(&evidence.join("review.json"), &review)?;
    let pending =
        client.prepare_model_registration(review.request.ok_or("model not registrable")?)?;
    // Retain each exact keyed request before its mutation; errors do not retry.
    retain(&evidence.join("pending-registration.json"), &pending)?;
    let registered = client.register_model(&pending).await?;
    retain(&evidence.join("registration.json"), &registered)?;
    let source = client.recipe_definition(&RecipeId::new(&args[1])?).await?;
    let pending = client.prepare_recipe_save(RecipeMutation::Clone {
        request: CloneRecipeRequest {
            compaction: None,
            harness: None,
            source: RecipeSource {
                execution_recipe_id: source.execution_recipe_id,
                definition_digest: source.definition_digest,
            },
            name: format!("{} on {}", source.name, args[2]),
            prompt: source.prompt,
            model_settings: Some(settings),
            model_registration: Some(registered.registration.registration_id),
        },
    })?;
    retain(&evidence.join("pending-save.json"), &pending)?;
    let saved = client.save_recipe(&pending).await?;
    retain(&evidence.join("saved.json"), &saved)?;
    let preview = client
        .preview(&saved.definition.execution_recipe_id, RunMode::Experiment)
        .await?;
    retain(&evidence.join("preview.json"), &preview)?;
    println!("{}", saved.definition.execution_recipe_id);
    Ok(())
}
