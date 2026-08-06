//! Fresh / stale / cold-start.
//!
//! This behaviour is documented for the customer's technical file, so it is
//! modelled explicitly rather than left implicit in a comparison.

use std::time::{Duration, SystemTime};

use ancre_types::RiskClass;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    /// Budget exceeded, control plane unreachable.
    Stale,
    /// No snapshot has ever been loaded.
    ColdStart,
}

#[derive(Debug, Clone)]
pub struct StalenessPolicy {
    pub budget: Duration,
    /// Default: on for `RiskClass::High`.
    pub fail_closed_on_stale: bool,
}

impl StalenessPolicy {
    #[must_use]
    pub fn freshness(&self, _built_at: SystemTime) -> Freshness {
        todo!("M2: elapsed vs budget, saturating on clock skew")
    }

    /// | State     | High risk        | Everything else                    |
    /// |-----------|------------------|------------------------------------|
    /// | Fresh     | serve            | serve                              |
    /// | Stale     | **503**          | serve, `StaleConfig` on every event |
    /// | ColdStart | **503**          | **503**                            |
    #[must_use]
    pub fn admits(&self, _f: Freshness, _risk: RiskClass) -> bool {
        todo!("M2")
    }
}
