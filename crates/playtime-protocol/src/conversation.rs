//! Saved, completed chat turns. These values confer no tool execution authority.

use playtime_core::{RequestId, RunId, Sha256Digest};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FailureHistory {
    pub records: Vec<Value>,
    pub omitted: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Exchange {
    pub request_id: RequestId,
    pub messages: Vec<Value>,
    pub failures: FailureHistory,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    pub root: Vec<Value>,
    pub summary: Option<Value>,
    pub failures: FailureHistory,
    pub exchanges: Vec<Exchange>,
    pub compactions: u64,
}

/// The controller exports this only after a successful turn and confirmed cleanup.
/// Sequence and request ordinals continue across turns, keeping retained failure
/// references distinct. Time and token allocations start afresh for each turn.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationCheckpoint {
    pub schema_version: u32,
    pub configuration: Sha256Digest,
    pub completed_turn: RunId,
    pub next_sequence: u64,
    pub next_request: u64,
    pub history: History,
}
