//! Fresh / stale / cold-start.
//!
//! This behaviour is documented for the customer's technical file, so it is
//! modelled explicitly rather than left implicit in a comparison.

use std::time::{Duration, Instant};

use ancre_types::RiskClass;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    Fresh,
    /// Budget exceeded — the control plane has not been reachable for longer
    /// than the customer's configured window.
    Stale,
    /// No snapshot has ever been loaded.
    ColdStart,
}

#[derive(Debug, Clone)]
pub struct StalenessPolicy {
    pub budget: Duration,
    /// Default: on. Turning it off is a governance event in its own right, and
    /// that log line is worth more than the setting.
    pub fail_closed_on_stale: bool,
}

impl Default for StalenessPolicy {
    fn default() -> Self {
        Self {
            budget: crate::DEFAULT_STALENESS_BUDGET,
            fail_closed_on_stale: true,
        }
    }
}

impl StalenessPolicy {
    /// Age is measured from when **this node** loaded the snapshot, on the
    /// local monotonic clock — not from the control plane's `built_at`.
    ///
    /// Two reasons, and the second is the one that matters:
    ///
    /// 1. `Instant` is monotonic, so an NTP step cannot make a fresh config
    ///    look stale, or a stale one look fresh.
    /// 2. Clock skew between two machines would otherwise feed into a
    ///    fail-closed decision. A gateway whose clock runs two minutes behind
    ///    the control plane would 503 every high-risk request under a
    ///    perfectly current config.
    ///
    /// It is also the more honest measurement of the claim being made. The
    /// customer-facing promise is "the maximum time a node can serve under a
    /// superseded configuration" — which is exactly time since this node last
    /// refreshed.
    #[must_use]
    pub fn freshness(&self, loaded_at: Instant) -> Freshness {
        if loaded_at.elapsed() > self.budget {
            Freshness::Stale
        } else {
            Freshness::Fresh
        }
    }

    /// Who gets served, in which state.
    ///
    /// | State     | High risk | Everything else                     |
    /// |-----------|-----------|-------------------------------------|
    /// | Fresh     | serve     | serve                               |
    /// | Stale     | **503**   | serve, `StaleConfig` on every event  |
    /// | ColdStart | **503**   | **503**                             |
    ///
    /// Cold start fails closed for *every* class, not just High: a node that
    /// has never loaded a snapshot cannot pin anything at all, so serving
    /// would produce events whose every pin is `unknown`. Better to refuse the
    /// request than to write evidence that proves nothing.
    #[must_use]
    pub fn admits(&self, f: Freshness, risk: RiskClass) -> bool {
        match f {
            Freshness::Fresh => true,
            Freshness::ColdStart => false,
            Freshness::Stale => !(self.fail_closed_on_stale && risk.fails_closed_by_default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> StalenessPolicy {
        StalenessPolicy {
            budget: Duration::from_millis(50),
            fail_closed_on_stale: true,
        }
    }

    #[test]
    fn a_just_loaded_snapshot_is_fresh() {
        assert_eq!(policy().freshness(Instant::now()), Freshness::Fresh);
    }

    #[test]
    fn a_snapshot_older_than_the_budget_is_stale() {
        let old = Instant::now()
            .checked_sub(Duration::from_millis(80))
            .expect("the process has been running for at least 80ms");
        assert_eq!(policy().freshness(old), Freshness::Stale);
    }

    /// Resolver spec §10, test 3.
    #[test]
    fn stale_plus_high_risk_is_refused() {
        assert!(!policy().admits(Freshness::Stale, RiskClass::High));
    }

    /// Resolver spec §10, test 4.
    #[test]
    fn stale_plus_lower_risk_is_served() {
        for risk in [
            RiskClass::Minimal,
            RiskClass::Transparency,
            RiskClass::Unclassified,
        ] {
            assert!(
                policy().admits(Freshness::Stale, risk),
                "{risk:?} must still be served when stale"
            );
        }
    }

    /// Resolver spec §10, test 7.
    #[test]
    fn cold_start_is_refused_for_every_risk_class() {
        for risk in [
            RiskClass::High,
            RiskClass::Transparency,
            RiskClass::Minimal,
            RiskClass::Unclassified,
        ] {
            assert!(
                !policy().admits(Freshness::ColdStart, risk),
                "{risk:?} must not be served before the first snapshot"
            );
        }
    }

    #[test]
    fn turning_fail_closed_off_serves_stale_high_risk_traffic() {
        // Configurable, as the spec requires. The gateway logs the attempt.
        let permissive = StalenessPolicy {
            fail_closed_on_stale: false,
            ..policy()
        };
        assert!(permissive.admits(Freshness::Stale, RiskClass::High));
        // And cold start is still refused — that one is not configurable.
        assert!(!permissive.admits(Freshness::ColdStart, RiskClass::High));
    }

    #[test]
    fn the_default_policy_fails_closed() {
        assert!(StalenessPolicy::default().fail_closed_on_stale);
    }
}
