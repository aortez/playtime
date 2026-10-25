//! Runtime validation of private policy proposals. No external adapter can
//! construct or execute these directly.

use super::*;
use crate::policy::{Facts, Proposal};

#[cfg(test)]
mod tests;

impl<I: IdSource> Controller<I> {
    pub(super) fn facts(&self) -> Facts {
        Facts {
            remaining_tokens: self.remaining_tokens(),
            session: self.session,
            turns: self.turns,
            handoff_attempts: self.handoff_attempts,
            current_handoff: self.current_handoff,
            parse_streak: self.parse_streak,
            session_parse_errors: self.session_parse_errors,
            ended: self.ended,
            finished: self.finished,
        }
    }

    /// The policy cannot dispatch. Validate each proposal against runtime facts
    /// and the frozen memento contract before creating an effect or changing a
    /// lifecycle boundary. Invalid internal proposals fail closed in pump().
    pub(super) fn apply_proposal(&mut self, proposal: Proposal) -> Result<(), InvalidValue> {
        if self.reason.is_some() || self.pending.is_some() || self.stage != Stage::Policy {
            return Err(InvalidValue(
                "policy proposed work outside an idle running controller",
            ));
        }
        let limits = &self.config.parameters.limits;
        let published = !self.active
            && self.checkpoint_attempted
            && self.last_checkpoint.as_ref().is_some_and(|checkpoint| {
                Some(&checkpoint.session_id) == self.session_id.as_ref()
                    && checkpoint.ordinal == self.session
            });
        match proposal {
            Proposal::StartSession => {
                if self.active
                    || self.session >= limits.sessions.get()
                    || self.finished
                    || self.remaining_tokens() == 0
                    || (self.session > 0 && !published)
                {
                    return Err(InvalidValue("policy proposed an unpermitted session start"));
                }
                self.start_session();
            }
            Proposal::Finish(reason) => {
                let permitted = match reason {
                    StopReason::AgentFinished => self.finished && self.ended,
                    StopReason::TokenLimit => !self.finished && self.remaining_tokens() == 0,
                    StopReason::SessionLimit => {
                        !self.finished
                            && self.remaining_tokens() > 0
                            && self.session >= limits.sessions.get()
                    }
                    _ => false,
                };
                if !published || !permitted {
                    return Err(InvalidValue(
                        "policy finish lacks its completion prerequisite or publication",
                    ));
                }
                self.stop(reason);
            }
            proposal => {
                if !self.active
                    || self.checkpoint_attempted
                    || self.reservation.is_some()
                    || self.tool.is_some()
                {
                    return Err(InvalidValue(
                        "policy proposed work without an available session",
                    ));
                }
                match proposal {
                    Proposal::Infer(inference) => {
                        if self.ended
                            || self.tool_calls_remaining != 0
                            || self.parse_streak > limits.tool_parse_retries
                            || (inference.handoff
                                && (self.phase != RunPhase::HandingOff
                                    || self.handoff_attempts >= 2))
                            || (!inference.handoff
                                && (self.phase != RunPhase::Working
                                    || self.turns >= limits.work_turns_per_session.get()
                                    || self.session_parse_errors
                                        >= contract::parse_ceiling(limits)))
                            || inference.tools != contract::tools(inference.handoff)
                        {
                            return Err(InvalidValue(
                                "policy inference exceeds frozen turn, retry, or tool capabilities",
                            ));
                        }
                        self.request(inference);
                    }
                    Proposal::Tool(request) => {
                        if self.tool_calls_remaining == 0 {
                            return Err(InvalidValue("policy tool has no admitted response call"));
                        }
                        self.tool_calls_remaining -= 1;
                        self.next_tool(request);
                    }
                    proposal => {
                        if self.tool_calls_remaining != 0 {
                            return Err(InvalidValue("policy transition abandoned response calls"));
                        }
                        match proposal {
                            Proposal::BeginHandoff => {
                                if self.phase != RunPhase::Working || self.ended {
                                    return Err(InvalidValue(
                                        "policy proposed an unpermitted handoff",
                                    ));
                                }
                                self.begin_handoff();
                            }
                            Proposal::ResponseComplete => self.after_response(true),
                            Proposal::Checkpoint { missing_handoff } => {
                                if missing_handoff {
                                    self.emit("handoff_missing", json!({}));
                                }
                                self.stage = Stage::Checkpoint;
                            }
                            _ => unreachable!("proposal handled above"),
                        }
                    }
                }
            }
        }
        Ok(())
    }
}
