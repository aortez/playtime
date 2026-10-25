//! Test-only contract probe, not a compaction algorithm or selectable harness.
use super::{Inference, Proposal, View};
use crate::{
    ToolRequest,
    context::{message, tool_message},
};
use playtime_core::{RequestPurpose, StopReason};
use serde_json::Value;
use std::collections::VecDeque;

#[derive(Default)]
enum Stage {
    #[default]
    Starting,
    Work,
    Calls,
    Maintenance,
    Capture,
    Published,
}

#[derive(Default)]
pub(crate) struct Probe {
    stage: Stage,
    messages: Vec<Value>,
    calls: VecDeque<Value>,
}

impl Probe {
    pub fn propose(&mut self, view: View<'_>) -> Proposal {
        match self.stage {
            Stage::Starting => Proposal::StartWorkspace,
            Stage::Work | Stage::Maintenance => {
                let config = view.config.workspace.as_ref().unwrap();
                if self.messages.is_empty() {
                    self.messages = vec![
                        message("system", &config.protocol),
                        message("user", &config.prompt),
                    ];
                }
                let purpose = if matches!(self.stage, Stage::Work) {
                    RequestPurpose::Work
                } else {
                    RequestPurpose::Compaction
                };
                Proposal::Infer(Inference {
                    purpose,
                    messages: self.messages.clone(),
                    tools: super::workspace_tools(purpose),
                })
            }
            Stage::Calls => self
                .calls
                .pop_front()
                .map_or(Proposal::ResponseComplete, |call| {
                    Proposal::Tool(ToolRequest {
                        name: call["function"]["name"]
                            .as_str()
                            .unwrap_or("unknown")
                            .into(),
                        arguments: call["function"]["arguments"].clone(),
                    })
                }),
            Stage::Capture => Proposal::CaptureWorkspace,
            Stage::Published => Proposal::Finish(StopReason::AgentFinished),
        }
    }
    pub fn started(&mut self) {
        self.stage = Stage::Work;
    }
    pub fn model_response(&mut self, message: Value) {
        if matches!(self.stage, Stage::Maintenance) {
            self.stage = Stage::Capture;
        } else {
            self.calls = message["tool_calls"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .into();
            self.stage = Stage::Calls;
        }
        self.messages.push(message);
    }
    pub fn tool_result(&mut self, name: &str, result: &Value) {
        self.messages.push(tool_message(name, result));
    }
    pub fn after_response(&mut self) {
        self.messages.push(message(
            "user",
            "Test-only maintenance request. Return a brief summary without tools.",
        ));
        self.stage = Stage::Maintenance;
    }
    pub fn published(&mut self) {
        self.stage = Stage::Published;
    }
    pub fn stopped(&mut self) {
        self.calls.clear();
    }
}
