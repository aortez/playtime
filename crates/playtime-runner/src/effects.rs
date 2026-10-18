//! Bounded replaceable effects for the one archive/controller loop. Dispatch is
//! called only after its intent receipt. Retirement may be asynchronous: the
//! owner must remain busy until it can safely accept finalization work.
use crate::Result;
use playtime_engine::{Checkpoint, Effect, Input, ToolPolicy, simulation::ScriptedAdapters};
use playtime_storage::CommittedFrontier;
use std::collections::BTreeMap;

pub enum CheckpointBytes<'a> {
    Scripted(&'a [u8]),
    Contents {
        workspace: &'a [u8],
        files: BTreeMap<&'a str, &'a [u8]>,
    },
}

pub trait Effects: Send {
    fn begin(&mut self, _started_ms: u64) -> Result<()> {
        Ok(())
    }
    fn policy(&self) -> ToolPolicy {
        ToolPolicy::Simulation
    }
    /// Drain bounded completion state without blocking the controller.
    fn poll(&mut self, _now: u64) {}
    fn active(&self) -> Option<u64>;
    fn next_input_ms(&self) -> Option<u64>;
    fn pop_input(&mut self) -> Option<(u64, Input)>;
    /// Request retirement, retaining external owners until their completion.
    fn cancel(&mut self);
    /// Propagate user cancellation without discarding a still-valid finalization reply.
    fn signal_cancel(&mut self) {}
    fn retiring(&self) -> bool {
        false
    }
    fn dispatch(&mut self, now: u64, effect: &Effect, confirmed: &CommittedFrontier) -> Result<()>;
    fn checkpoint<'a>(&'a self, checkpoint: &Checkpoint) -> Result<CheckpointBytes<'a>>;
}
impl Effects for ScriptedAdapters {
    fn active(&self) -> Option<u64> {
        self.active()
    }
    fn next_input_ms(&self) -> Option<u64> {
        self.next_input_ms()
    }
    fn pop_input(&mut self) -> Option<(u64, Input)> {
        self.pop_input()
    }
    fn cancel(&mut self) {
        self.cancel();
    }
    fn dispatch(&mut self, now: u64, effect: &Effect, _: &CommittedFrontier) -> Result<()> {
        Ok(self.dispatch(now, effect)?)
    }
    fn checkpoint<'a>(&'a self, checkpoint: &Checkpoint) -> Result<CheckpointBytes<'a>> {
        Ok(CheckpointBytes::Scripted(
            &self.artifacts()[checkpoint.snapshot.workspace.path.as_str()],
        ))
    }
}
