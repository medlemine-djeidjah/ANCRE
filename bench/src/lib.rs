//! Shared fixture builders for the benches.
//!
//! Kept out of the crates themselves so a 50k-route fixture generator never
//! ends up compiled into the gateway binary.

/// 10k systems / 50k routes — the shape the snapshot-build target is stated
/// against (resolver spec §9).
#[must_use]
pub fn large_snapshot_spec() -> ancre_types::SnapshotSpec {
    todo!("M2")
}
