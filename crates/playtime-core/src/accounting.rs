use std::{fmt, num::NonZeroU64};

use serde::{Deserialize, Serialize};

/// The accounting outcome of one model request, including reasoning tokens.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum GenerationUsage {
    Known {
        generated_tokens: u64,
    },
    /// Missing usage retains the request's entire dispatched generation cap.
    Unknown {
        reserved_tokens: NonZeroU64,
    },
}

/// Aggregates request outcomes without treating unknown generation as zero.
///
/// Each request must be recorded exactly once. This is an outcome accumulator,
/// not a request-admission or in-flight reservation mechanism; the controller
/// must reserve the allowance before dispatch and settle it on completion.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UsageTotals {
    known_generated_tokens: u64,
    unknown_reserved_tokens: u64,
    unknown_requests: u64,
}

impl UsageTotals {
    /// Overflow leaves the previous totals intact.
    pub fn record(&mut self, usage: GenerationUsage) -> Result<(), AccountingOverflow> {
        let mut next = *self;
        match usage {
            GenerationUsage::Known { generated_tokens } => {
                next.known_generated_tokens = next
                    .known_generated_tokens
                    .checked_add(generated_tokens)
                    .ok_or(AccountingOverflow)?;
            }
            GenerationUsage::Unknown { reserved_tokens } => {
                next.unknown_reserved_tokens = next
                    .unknown_reserved_tokens
                    .checked_add(reserved_tokens.get())
                    .ok_or(AccountingOverflow)?;
                next.unknown_requests = next
                    .unknown_requests
                    .checked_add(1)
                    .ok_or(AccountingOverflow)?;
            }
        }
        next.known_generated_tokens
            .checked_add(next.unknown_reserved_tokens)
            .ok_or(AccountingOverflow)?;
        *self = next;
        Ok(())
    }

    pub fn known_generated_tokens(&self) -> u64 {
        self.known_generated_tokens
    }

    pub fn unknown_reserved_tokens(&self) -> u64 {
        self.unknown_reserved_tokens
    }

    pub fn unknown_requests(&self) -> u64 {
        self.unknown_requests
    }

    pub fn budget_charged_tokens(&self) -> u64 {
        // record() verifies the combined total before publishing an update.
        self.known_generated_tokens + self.unknown_reserved_tokens
    }

    pub fn usage_complete(&self) -> bool {
        self.unknown_requests == 0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AccountingOverflow;

impl fmt::Display for AccountingOverflow {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("generation accounting exceeds u64 capacity")
    }
}

impl std::error::Error for AccountingOverflow {}

#[cfg(test)]
mod tests {
    use super::*;

    fn unknown(tokens: u64) -> GenerationUsage {
        GenerationUsage::Unknown {
            reserved_tokens: NonZeroU64::new(tokens).unwrap(),
        }
    }

    #[test]
    fn later_success_does_not_erase_unknown_usage() {
        let mut totals = UsageTotals::default();
        totals.record(unknown(100)).unwrap();
        for _ in 0..2 {
            totals
                .record(GenerationUsage::Known {
                    generated_tokens: 5,
                })
                .unwrap();
        }
        // Mirrors the synthetic Python recovery test: one failed work request,
        // a successful handoff, then a successful request in a fresh session.
        assert_eq!(totals.known_generated_tokens(), 10);
        assert_eq!(totals.unknown_reserved_tokens(), 100);
        assert_eq!(totals.unknown_requests(), 1);
        assert_eq!(totals.budget_charged_tokens(), 110);
        assert!(!totals.usage_complete());
    }

    #[test]
    fn measured_zero_is_complete_but_an_unknown_request_is_not() {
        let mut totals = UsageTotals::default();
        totals
            .record(GenerationUsage::Known {
                generated_tokens: 0,
            })
            .unwrap();
        assert_eq!(totals.budget_charged_tokens(), 0);
        assert!(totals.usage_complete());

        totals.record(unknown(50)).unwrap();
        assert_eq!(totals.known_generated_tokens(), 0);
        assert_eq!(totals.budget_charged_tokens(), 50);
        assert!(!totals.usage_complete());
    }

    #[test]
    fn overflow_does_not_partially_update_totals() {
        for initial in [
            GenerationUsage::Known {
                generated_tokens: u64::MAX,
            },
            unknown(u64::MAX),
        ] {
            for extra in [
                GenerationUsage::Known {
                    generated_tokens: 1,
                },
                unknown(1),
            ] {
                let mut totals = UsageTotals::default();
                totals.record(initial).unwrap();
                let before = totals;
                assert_eq!(totals.record(extra), Err(AccountingOverflow));
                assert_eq!(totals, before);
                assert_eq!(totals.budget_charged_tokens(), u64::MAX);
            }
        }
    }
}
