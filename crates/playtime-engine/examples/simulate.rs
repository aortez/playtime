use std::{fs::File, io::Read};

use playtime_engine::simulation::{Scenario, run};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .ok_or("usage: simulate SCENARIO.json")?;
    let mut bytes = Vec::new();
    File::open(path)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err("scenario exceeds 1 MiB".into());
    }
    let scenario: Scenario = serde_json::from_slice(&bytes)?;
    println!("{}", serde_json::to_string_pretty(&run(scenario)?)?);
    Ok(())
}
