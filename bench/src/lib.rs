//! Shared harness for the benches.
//!
//! Kept out of the crates themselves so a 50k-route fixture generator never
//! ends up compiled into the gateway binary.
//!
//! Percentile arithmetic is float-and-index work by nature; the numeric-cast
//! lints are allowed here and nowhere else in the workspace.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]

use std::time::Duration;

/// Percentiles over raw per-call samples.
///
/// Criterion reports a mean and a slope, which is the right tool for catching
/// regressions but the wrong one for the claim being sold. The published
/// number is a **p99**, and a mean hides exactly the tail a customer's
/// platform team will measure. So the gate reads raw samples.
#[derive(Debug, Clone, Copy)]
pub struct Percentiles {
    pub n: usize,
    pub p50: Duration,
    pub p90: Duration,
    pub p99: Duration,
    pub p999: Duration,
    pub max: Duration,
}

impl Percentiles {
    /// `samples` is consumed and sorted in place.
    #[must_use]
    pub fn from_samples(samples: &mut [Duration]) -> Self {
        assert!(!samples.is_empty(), "no samples");
        samples.sort_unstable();
        let at = |q: f64| {
            let i = ((samples.len() as f64 - 1.0) * q).round() as usize;
            samples[i]
        };
        Self {
            n: samples.len(),
            p50: at(0.50),
            p90: at(0.90),
            p99: at(0.99),
            p999: at(0.999),
            max: samples[samples.len() - 1],
        }
    }
}

impl std::fmt::Display for Percentiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "n={:<9} p50={:>9.0?}  p90={:>9.0?}  p99={:>9.0?}  p99.9={:>9.0?}  max={:>9.0?}",
            self.n, self.p50, self.p90, self.p99, self.p999, self.max
        )
    }
}

/// A pass/fail line against a stated target.
#[must_use]
pub fn check(label: &str, actual: Duration, target: Duration) -> bool {
    let ok = actual <= target;
    println!(
        "  [{}] {label:<44} {:>9.0?} (target {:.0?})",
        if ok { "PASS" } else { "FAIL" },
        actual,
        target
    );
    ok
}

/// Cost of `Instant::now()` itself, measured on the same machine.
///
/// At a 2µs target the timer is a percent or two of the measurement. Reporting
/// it means the numbers can be read honestly rather than defended.
#[must_use]
pub fn timer_overhead(iters: usize) -> Duration {
    let mut samples = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = std::time::Instant::now();
        samples.push(t.elapsed());
    }
    Percentiles::from_samples(&mut samples).p50
}
