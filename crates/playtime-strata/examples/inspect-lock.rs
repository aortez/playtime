//! Explicit discovery for a caller-owned endpoint, with independently verified
//! host evidence. No automatic lock refresh, server startup or host access.
use playtime_strata::*;
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    os::unix::fs::OpenOptionsExt,
    time::Duration,
};
use tokio::time::Instant;

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 4 {
        return Err(
            "usage: inspect-lock LOOPBACK_ENDPOINT HOST_EVIDENCE_JSON NEW_LOCK_JSON".into(),
        );
    }
    let host: Value = serde_json::from_slice(&fs::read(&args[2])?)?;
    let seen = inspect_endpoint(
        &Endpoint::parse(&args[1])?,
        &Limits::default(),
        Instant::now() + Duration::from_secs(15),
        &CancellationToken::new(),
    )
    .await?;
    if seen.in_flight != 0 {
        return Err("endpoint is busy".into());
    }
    let lock = ModelLock {
        schema_version: 1,
        identity: seen.identity,
        host: serde_json::from_value(host["host"].clone())?,
        settings: Settings::default(),
    };
    let fingerprint = lock.fingerprint()?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&args[3])?;
    file.write_all(&serde_json::to_vec_pretty(&lock)?)?;
    file.sync_all()?;
    println!(
        "{}",
        json!({"model":lock.identity.model,"context":lock.identity.context_tokens,"fingerprint":fingerprint,"started":seen.started})
    );
    Ok(())
}
