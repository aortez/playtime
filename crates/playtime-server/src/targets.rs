//! Named inference targets supplied by the service owner; no inference during loading.
use playtime_protocol::api::InferenceTargets;
use std::{io::Read, path::Path};
pub fn load(path: &Path) -> Result<InferenceTargets, Box<dyn std::error::Error>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(65537)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 65536 {
        return Err("target configuration exceeds 64 KiB".into());
    }
    let targets: InferenceTargets = serde_json::from_slice(&bytes)?;
    validate(&targets)?;
    Ok(targets)
}
pub fn validate(targets: &InferenceTargets) -> Result<(), &'static str> {
    if !targets.valid() {
        return Err("invalid or duplicate inference targets");
    }
    for target in &targets.targets {
        playtime_ollama::Endpoint::parse(&target.endpoint)
            .map_err(|_| "invalid target endpoint")?;
    }
    Ok(())
}
