//! Offline staging helper. Never contacts Ollama or reads host credentials.
use std::io::Read;
fn main() {
    if run().is_err() {
        eprintln!("unsupported frozen profile");
        std::process::exit(1);
    }
}
fn run() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args_os().len() != 1 {
        return Err("no arguments supported".into());
    }
    let mut bytes = vec![];
    std::io::stdin()
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    playtime_ollama::ModelLock::parse(&bytes)?;
    let lock = playtime_backend_guard::digest(&bytes);
    let settings = playtime_backend_guard::digest(&serde_json::to_vec(
        &playtime_ollama::EffectiveSettings::default(),
    )?);
    let profile = playtime_backend_guard::digest(
        format!("{}:{}", lock.as_str(), settings.as_str()).as_bytes(),
    );
    println!(
        "{}",
        serde_json::json!({"lock_sha256":lock,"settings_sha256":settings,"profile_sha256":profile})
    );
    Ok(())
}
