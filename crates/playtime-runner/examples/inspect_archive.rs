use std::env;

use playtime_runner::ArchiveQueries;
use playtime_storage::StorageLimits;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = env::args().skip(1).collect();
    if arguments.len() != 2 || arguments[0] != "--run" {
        return Err("usage: inspect_archive --run <archive-directory>".into());
    }
    let queries = ArchiveQueries::offline(&arguments[1], StorageLimits::default())?;
    let mut checkpoints = Vec::new();
    for checkpoint in queries.checkpoints()? {
        let content = if let Some(index) = &checkpoint.index {
            let evidence = |value: &Option<playtime_protocol::bridge::EvidenceContent>| -> Result<_, playtime_runner::Error> {
                value.as_ref().map(|value| queries.content(&value.text).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())).transpose()
            };
            serde_json::json!({
                "incoming_memory":String::from_utf8(queries.content(&index.incoming_memory.content)?)?,
                "outgoing_memory":String::from_utf8(queries.content(&index.outgoing_memory.content)?)?,
                "incoming_evidence":evidence(&index.incoming_evidence)?,
                "outgoing_evidence":evidence(&index.outgoing_evidence)?,
            })
        } else {
            serde_json::json!({"availability":"index_not_recorded"})
        };
        checkpoints.push(serde_json::json!({"checkpoint":checkpoint,"content":content}));
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "run":queries.run()?,"inputs":queries.inputs(),"checkpoints":checkpoints,
            "verified_content":queries.snapshot().inspection().referenced_content,
            "worker": {"executable_bytes_verified":true,"execution_checked":false}
        }))?
    );
    Ok(())
}
