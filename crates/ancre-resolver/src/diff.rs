//! Consecutive-generation diff.
//!
//! Article 12(2)(a) wants situations that *may* constitute a substantial
//! modification to be identifiable from the logs. This produces that list.
//!
//! **It surfaces candidates and never declares conclusions** (PRD §6.5). Under
//! the Act a substantial modification can reset a grandfathering position and
//! pull a system back into scope — that is a legal determination. The product
//! flags; the human decides; the decision is logged. Every name in this module
//! is worded to keep that distinction visible in the code, not just in the UI.

use ancre_types::{ChangeClass, ConfigSnapshot, SystemConfig};

/// Feeds the `config.generation.applied` event.
///
/// `propagation_ms` is the evidence for the bounded-staleness claim. Its p99
/// goes in the sales deck *and* in the customer's Article 11 documentation, so
/// it is measured, not asserted.
#[derive(Debug, Clone)]
pub struct ReloadOutcome {
    pub from_generation: u64,
    pub to_generation: u64,
    /// Control-plane build time to local install time.
    ///
    /// Crosses two machines' clocks, so it is an estimate and is reported as
    /// one. Negative skew is clamped to zero rather than wrapping.
    pub propagation_ms: u32,
    pub changes: Vec<SystemChange>,
    pub systems_added: Vec<String>,
    pub systems_removed: Vec<String>,
}

impl ReloadOutcome {
    /// The first snapshot after cold start. Not a change — there is nothing to
    /// compare against, and reporting every system as "added" on every process
    /// restart would bury the one time it actually means something.
    #[must_use]
    pub fn first_snapshot(next: &ConfigSnapshot) -> Self {
        Self {
            from_generation: 0,
            to_generation: next.generation,
            propagation_ms: propagation_ms(next),
            changes: Vec::new(),
            systems_added: Vec::new(),
            systems_removed: Vec::new(),
        }
    }

    /// Systems whose change **may** constitute a substantial modification.
    ///
    /// Alert-worthy. Never auto-classified — the caller's job is to raise it
    /// for review, worded as "may constitute a substantial modification —
    /// review required", not to act on it.
    #[must_use]
    pub fn substantial_candidates(&self) -> Vec<&SystemChange> {
        self.changes
            .iter()
            .filter(|c| c.class == ChangeClass::Substantial)
            .collect()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.systems_added.is_empty() && self.systems_removed.is_empty()
    }

    /// Flat list for the audit event's `changed_fields`.
    #[must_use]
    pub fn changed_fields(&self) -> Vec<String> {
        self.changes
            .iter()
            .flat_map(|c| {
                c.fields
                    .iter()
                    .map(move |f| format!("{}.{}", c.system_id, f))
            })
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemChange {
    pub system_id: String,
    pub class: ChangeClass,
    /// Which fields moved. Named, because "this system changed" is not a
    /// reviewable statement and "its model_id changed" is.
    pub fields: Vec<&'static str>,
}

/// Resolver spec §8.
///
/// The spec's pseudocode compares `prev.model_id` as if a system had one
/// model. It has one per route, so "the model changed" means the ordered list
/// of route models changed — adding a route that sends some traffic to a
/// different model is exactly as substantial as swapping the only one.
///
/// Prompt changes are `Material` rather than `Substantial`. That is a
/// judgement call: flag it, let the customer's own policy decide, and log the
/// decision.
#[must_use]
pub fn classify_change(prev: &SystemConfig, next: &SystemConfig) -> ChangeClass {
    if routes_differ_on(prev, next, |r| &r.model_id)
        || major(&prev.system_version) != major(&next.system_version)
    {
        return ChangeClass::Substantial;
    }
    if prev.policy_version != next.policy_version
        || routes_differ_on(prev, next, |r| &r.prompt_version)
    {
        return ChangeClass::Material;
    }
    ChangeClass::Minor
}

/// Compare one field across the ordered route lists. A different length is a
/// difference: routes are first-match-wins, so their order and count are
/// semantic.
fn routes_differ_on<F>(prev: &SystemConfig, next: &SystemConfig, field: F) -> bool
where
    F: Fn(&ancre_types::Route) -> &std::sync::Arc<str>,
{
    prev.routes.len() != next.routes.len()
        || prev
            .routes
            .iter()
            .zip(&next.routes)
            .any(|(a, b)| field(a) != field(b))
}

/// Which fields moved, for the audit event.
fn changed_fields(prev: &SystemConfig, next: &SystemConfig) -> Vec<&'static str> {
    let mut f = Vec::new();
    if routes_differ_on(prev, next, |r| &r.model_id) {
        f.push("model_id");
    }
    if routes_differ_on(prev, next, |r| &r.model_version) {
        f.push("model_version");
    }
    if routes_differ_on(prev, next, |r| &r.prompt_version) {
        f.push("prompt_version");
    }
    if prev.system_version != next.system_version {
        f.push("system_version");
    }
    if prev.policy_version != next.policy_version {
        f.push("policy_version");
    }
    if prev.ifu_version != next.ifu_version {
        f.push("ifu_version");
    }
    if prev.risk_class != next.risk_class {
        f.push("risk_class");
    }
    if prev.routes.len() != next.routes.len() {
        f.push("routes");
    }
    f
}

pub(crate) fn diff(prev: &ConfigSnapshot, next: &ConfigSnapshot) -> ReloadOutcome {
    let mut changes = Vec::new();
    let mut systems_added = Vec::new();
    let mut systems_removed = Vec::new();

    for (id, next_sys) in next.systems() {
        match prev.system(id) {
            Some(prev_sys) => {
                let fields = changed_fields(prev_sys, next_sys);
                if !fields.is_empty() {
                    changes.push(SystemChange {
                        system_id: id.to_string(),
                        class: classify_change(prev_sys, next_sys),
                        fields,
                    });
                }
            }
            None => systems_added.push(id.to_string()),
        }
    }

    for (id, _) in prev.systems() {
        if next.system(id).is_none() {
            systems_removed.push(id.to_string());
        }
    }

    // Stable output: two nodes applying the same generation must produce the
    // same event, and `systems()` iterates a HashMap.
    changes.sort_by(|a, b| a.system_id.cmp(&b.system_id));
    systems_added.sort();
    systems_removed.sort();

    ReloadOutcome {
        from_generation: prev.generation,
        to_generation: next.generation,
        propagation_ms: propagation_ms(next),
        changes,
        systems_added,
        systems_removed,
    }
}

fn propagation_ms(next: &ConfigSnapshot) -> u32 {
    next.built_at
        .elapsed()
        .ok()
        .and_then(|d| u32::try_from(d.as_millis()).ok())
        .unwrap_or(0)
}

/// Semver major. A version that does not parse compares as itself, so a
/// malformed version never silently reads as "unchanged".
fn major(v: &str) -> &str {
    v.split('.').next().unwrap_or(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    #[test]
    fn a_model_swap_is_a_substantial_candidate() {
        let prev = testing::snapshot(41);
        let mut spec = testing::spec(42);
        spec.systems[0].routes[0].model_id = "claude-sonnet-5".into();
        let next = ancre_types::ConfigSnapshot::build(spec).unwrap();

        let outcome = diff(&prev, &next);
        let candidates = outcome.substantial_candidates();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].system_id, "hr-screening");
        assert!(candidates[0].fields.contains(&"model_id"));
    }

    #[test]
    fn a_major_version_bump_is_a_substantial_candidate() {
        let prev = testing::snapshot(41);
        let mut spec = testing::spec(42);
        spec.systems[0].system_version = "3.0.0".into();
        let next = ancre_types::ConfigSnapshot::build(spec).unwrap();

        assert_eq!(diff(&prev, &next).substantial_candidates().len(), 1);
    }

    #[test]
    fn a_minor_version_bump_is_not_substantial() {
        let prev = testing::snapshot(41);
        let mut spec = testing::spec(42);
        spec.systems[0].system_version = "2.2.0".into();
        let next = ancre_types::ConfigSnapshot::build(spec).unwrap();

        let outcome = diff(&prev, &next);
        assert!(outcome.substantial_candidates().is_empty());
        assert_eq!(outcome.changes[0].class, ChangeClass::Minor);
    }

    #[test]
    fn a_prompt_change_is_material_not_substantial() {
        let prev = testing::snapshot(41);
        let mut spec = testing::spec(42);
        spec.systems[0].routes[0].prompt_version = "b3:different".into();
        let next = ancre_types::ConfigSnapshot::build(spec).unwrap();

        let outcome = diff(&prev, &next);
        assert_eq!(outcome.changes[0].class, ChangeClass::Material);
        assert!(outcome.substantial_candidates().is_empty());
    }

    #[test]
    fn an_unchanged_reload_reports_nothing() {
        let prev = testing::snapshot(41);
        let next = testing::snapshot(42);
        assert!(diff(&prev, &next).is_empty());
    }

    #[test]
    fn added_and_removed_systems_are_reported_separately() {
        let prev = testing::snapshot(41);
        let mut spec = testing::spec(42);
        // The key must go with the system: `build` refuses a key bound to a
        // system that no longer exists, which is the validation doing its job.
        spec.systems.remove(1);
        spec.keys.remove(1);
        let next = ancre_types::ConfigSnapshot::build(spec).unwrap();

        let outcome = diff(&prev, &next);
        assert_eq!(outcome.systems_removed, vec!["credit-scoring"]);
        assert!(outcome.systems_added.is_empty());
    }

    #[test]
    fn the_diff_is_stable_across_hashmap_ordering() {
        // `systems()` iterates a HashMap, so without the sort two nodes could
        // emit the same change list in different orders — and the event would
        // differ between nodes that applied the same generation.
        let prev = testing::snapshot(41);
        let mut spec = testing::spec(42);
        spec.systems[0].routes[0].model_id = "claude-sonnet-5".into();
        spec.systems[1].routes[0].model_id = "claude-sonnet-5".into();
        let next = ancre_types::ConfigSnapshot::build(spec).unwrap();

        let first = diff(&prev, &next);
        for _ in 0..50 {
            assert_eq!(diff(&prev, &next).changes, first.changes);
        }
        assert_eq!(first.changes[0].system_id, "credit-scoring");
    }

    #[test]
    fn major_handles_a_malformed_version() {
        assert_eq!(major("2.1.0"), "2");
        assert_eq!(major("not-a-version"), "not-a-version");
        assert_eq!(major(""), "");
    }
}
