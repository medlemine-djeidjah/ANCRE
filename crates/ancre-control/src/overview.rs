//! What a chain looks like from above, for the dashboard.
//!
//! Everything here is an **aggregate**, computed in the store rather than by
//! reading a chain into the control plane. That is not only about speed: the
//! export endpoint already streams page by page precisely so resident memory
//! is bounded by the page and not by the chain, and a summary endpoint that
//! quietly loaded ten million events to count four risk flags would undo it
//! (`docs/deferred.md`, D1).
//!
//! ## What a summary is not
//!
//! It is **not a verification**. Nothing in here re-hashes anything, and the
//! dashboard is careful to say so: a green tick computed by the same server
//! that serves the events is worth nothing to an auditor, because the whole
//! product thesis is that the server is not trusted. The summary tells you
//! where to look; `ancre-verify` tells you whether to believe it.

use ancre_chain::ChainId;
use serde::{Deserialize, Serialize};

use crate::registry::ControlError;

/// One row in the chain list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChainListing {
    pub tenant_id: String,
    pub system_id: String,
    /// Highest `seq` written. Also the event count, because `seq` is gapless
    /// per chain by construction — if these two ever disagree the chain has a
    /// hole, which is a finding rather than a display bug.
    pub head_seq: u64,
    pub event_count: u64,
}

/// The counted shape of one chain.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ChainSummary {
    pub tenant_id: String,
    pub system_id: String,
    pub head_seq: u64,
    pub event_count: u64,
    /// Microseconds, matching `Timestamp`. `None` for an empty chain.
    pub first_event_at: Option<i64>,
    pub last_event_at: Option<i64>,
    /// Descending by count, so the dashboard can show the top few without
    /// deciding an order of its own.
    pub risk_flags: Vec<FlagCount>,
    pub event_types: Vec<FlagCount>,
    pub outcomes: Vec<FlagCount>,
    /// Distinct `config_generation` values seen in this chain — how many
    /// configurations this system has served under.
    pub generations: u64,
    /// Distinct `model_version` values, which is the question a
    /// substantial-modification review opens with.
    pub model_versions: Vec<FlagCount>,
    /// **`unknown` or `unresolved:` in any pin.** The countable gap, and the
    /// number an invoice is built from (PRD §6.3).
    pub events_with_gaps: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FlagCount {
    pub name: String,
    pub count: u64,
}

/// Aggregates over the event store.
///
/// A separate trait from `ChainSource` and `ChainExport` for the reason the
/// others are separate: these are the only methods a dashboard needs, and a
/// fake that returns fixed counts is enough to test every handler that uses
/// them.
pub trait ChainOverview: Send + Sync + 'static {
    fn listings(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<ChainListing>, ControlError>> + Send;

    fn summary(
        &self,
        chain: &ChainId,
    ) -> impl std::future::Future<Output = Result<ChainSummary, ControlError>> + Send;
}

impl<T: ChainOverview> ChainOverview for std::sync::Arc<T> {
    fn listings(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<ChainListing>, ControlError>> + Send {
        (**self).listings()
    }

    fn summary(
        &self,
        chain: &ChainId,
    ) -> impl std::future::Future<Output = Result<ChainSummary, ControlError>> + Send {
        (**self).summary(chain)
    }
}

#[cfg(feature = "testing")]
pub mod testing {
    //! A fixed overview, so the handlers can be tested without ClickHouse.

    use super::{ChainListing, ChainOverview, ChainSummary, ControlError, FlagCount};
    use ancre_chain::ChainId;

    #[derive(Debug, Default)]
    pub struct MemoryOverview {
        pub chains: Vec<ChainListing>,
    }

    impl MemoryOverview {
        #[must_use]
        pub fn with_one(tenant_id: &str, system_id: &str, head: u64) -> Self {
            Self {
                chains: vec![ChainListing {
                    tenant_id: tenant_id.into(),
                    system_id: system_id.into(),
                    head_seq: head,
                    event_count: head,
                }],
            }
        }
    }

    impl ChainOverview for MemoryOverview {
        async fn listings(&self) -> Result<Vec<ChainListing>, ControlError> {
            Ok(self.chains.clone())
        }

        async fn summary(&self, chain: &ChainId) -> Result<ChainSummary, ControlError> {
            let found = self
                .chains
                .iter()
                .find(|c| c.tenant_id == chain.tenant_id && c.system_id == chain.system_id);

            Ok(ChainSummary {
                tenant_id: chain.tenant_id.clone(),
                system_id: chain.system_id.clone(),
                head_seq: found.map_or(0, |c| c.head_seq),
                event_count: found.map_or(0, |c| c.event_count),
                first_event_at: found.map(|_| 1_700_000_000_000_000),
                last_event_at: found.map(|_| 1_700_000_100_000_000),
                risk_flags: vec![FlagCount {
                    name: "unpinned_model".into(),
                    count: 1,
                }],
                event_types: vec![FlagCount {
                    name: "llm.request".into(),
                    count: found.map_or(0, |c| c.event_count),
                }],
                outcomes: vec![FlagCount {
                    name: "ok".into(),
                    count: found.map_or(0, |c| c.event_count),
                }],
                generations: 1,
                model_versions: vec![FlagCount {
                    name: "gpt-4o-2024-08-06".into(),
                    count: found.map_or(0, |c| c.event_count),
                }],
                events_with_gaps: 0,
            })
        }
    }
}
