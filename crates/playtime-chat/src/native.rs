use crate::runtime::Effects;
use playtime_docker::{
    self as docker,
    project::{ProjectJournal, ProjectProposal, ProjectWorkspace},
};
use playtime_engine::{Effect, EffectKind, Outcome, ToolRequest};
use playtime_local::PrivateDirectory;
use playtime_strata::{Client, RemoteState};
use serde_json::Value;
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::{
    sync::{Mutex, mpsc},
    time::Instant,
};
use tokio_util::sync::CancellationToken;

pub struct Journal(pub PathBuf);
#[async_trait::async_trait]
impl ProjectJournal for Journal {
    async fn commit(&self, p: &ProjectProposal) -> docker::Result<()> {
        let path = self.0.clone();
        let p = p.clone();
        tokio::task::spawn_blocking(move || {
            PrivateDirectory::open(&path, false)?.write_new(
                &format!("{}.json", p.container.name),
                &serde_json::to_vec(&p)?,
            )
        })
        .await
        .map_err(|_| journal_error())?
        .map_err(|_| journal_error())
    }
}
fn journal_error() -> docker::Error {
    docker::Error {
        failure: docker::Failure::Journal,
        operation: "chat_ownership",
        detail: "could not durably commit project ownership",
    }
}

pub struct Model {
    pub client: Client,
    /// Uncertainty is cleared only by returning to the external server owner.
    pub fenced: bool,
}
pub struct NativeEffects {
    pub model: Arc<Mutex<Model>>,
    pub workspace: ProjectWorkspace,
    pub output_bytes: u32,
    pub return_overhead: Duration,
}
#[async_trait::async_trait]
impl Effects for NativeEffects {
    async fn execute(
        &mut self,
        effect: &Effect,
        deadline: Instant,
        cancel: CancellationToken,
        chunks: mpsc::Sender<Value>,
    ) -> Outcome {
        match &effect.kind {
            EffectKind::StartWorkspace => {
                match self.workspace.start_workspace(deadline, cancel).await {
                    Ok(()) => Outcome::WorkspaceStarted,
                    Err(e) => Outcome::Failed(e.to_string()),
                }
            }
            EffectKind::Tool(request) => {
                let tool = match tool(request) {
                    Ok(tool) => tool,
                    Err(error) => return Outcome::Failed(error),
                };
                let limit = deadline
                    .saturating_duration_since(Instant::now())
                    .saturating_sub(self.return_overhead)
                    .as_secs_f64();
                match self
                    .workspace
                    .tool(
                        tool,
                        docker::ToolLimits {
                            command_seconds: limit,
                            output_bytes: self.output_bytes,
                        },
                        deadline,
                        cancel,
                    )
                    .await
                {
                    Ok(reply) => {
                        Outcome::Tool(serde_json::to_value(reply).expect("tool serialization"))
                    }
                    Err(error) => Outcome::Failed(error.to_string()),
                }
            }
            EffectKind::Model(request) => {
                let mut model = self.model.lock().await;
                if model.fenced {
                    return Outcome::Failed(
                        "prior inference is unresolved; return to the server owner".into(),
                    );
                }
                model.fenced = true;
                let (tx, mut rx) = playtime_strata::events();
                let forward_cancel = cancel.clone();
                let forward = async move {
                    while let Some(event) = rx.recv().await {
                        if let Some(chunk) = event.controller_chunk() {
                            tokio::select! {biased;
                                _=forward_cancel.cancelled()=>break,
                                _=tokio::time::sleep_until(deadline)=>break,
                                sent=chunks.send(chunk)=>if sent.is_err() {forward_cancel.cancel();break;},
                            }
                        }
                    }
                };
                let (result, ()) = tokio::join!(
                    model.client.complete(request, deadline, &cancel, tx),
                    forward
                );
                model.fenced = result.remote == RemoteState::Uncertain;
                result.into_outcome()
            }
            EffectKind::Cleanup => Outcome::Cleaned(self.cleanup(deadline).await),
            _ => Outcome::Failed("effect is not supported by the project chat adapter".into()),
        }
    }
    async fn cleanup(&mut self, deadline: Instant) -> Vec<String> {
        self.workspace
            .cleanup(deadline, CancellationToken::new())
            .await
            .resources
            .into_iter()
            .filter_map(|r| match r {
                docker::ResourceOutcome::Residual { name, error, .. } => {
                    Some(format!("{name}: {error}"))
                }
                _ => None,
            })
            .collect()
    }
}
fn tool(request: &ToolRequest) -> std::result::Result<docker::Tool, String> {
    let a = &request.arguments;
    let string = |key| {
        a[key]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| format!("invalid {key} tool argument"))
    };
    Ok(match request.name.as_str() {
        "exec" => docker::Tool::Exec {
            command: string("command")?,
            cwd: a["cwd"].as_str().map(str::to_owned),
            timeout_seconds: a["timeout_seconds"].as_f64(),
        },
        "read_file" => docker::Tool::ReadFile {
            path: string("path")?,
            offset_bytes: a["offset_bytes"].as_u64().unwrap_or(0),
        },
        "write_file" => docker::Tool::WriteFile {
            path: string("path")?,
            content: string("content")?,
            append: a["append"].as_bool().unwrap_or(false),
        },
        _ => return Err("unsupported project tool".into()),
    })
}
