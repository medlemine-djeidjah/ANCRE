//! The ClickHouse `ChainSource`.
//!
//! Read-only, and deliberately so: the control plane signs what the ingester
//! wrote and must never be able to write it. Three queries, one of which is
//! load-bearing in a way the other two are not — `leaves` is ordered, because
//! the tree root is order-dependent and an unordered read produces a signature
//! over a root no verifier will ever reproduce. That failure surfaces at audit
//! time rather than at write time, which is the worst possible delay.

use ancre_canon::Hash32;
use ancre_chain::ChainId;
use ancre_ingester::clickhouse::AuditEventRow;
use ancre_types::AuditEvent;
use clickhouse::Row;
use serde::Deserialize;

use crate::checkpointer::ChainSource;
use crate::export::ChainExport;
use crate::registry::ControlError;

#[derive(Debug, Row, Deserialize)]
struct ChainRow {
    tenant_id: String,
    system_id: String,
}

/// Named fields rather than a tuple: the client's `Row` derive carries column
/// names into the wire format, and a tuple has none to carry. The alias is
/// `head_seq` and not `seq` for the same reason the ingester's is — aliasing an
/// aggregate back to the column it reads makes ClickHouse resolve the inner
/// name to the alias.
#[derive(Debug, Row, Deserialize)]
struct HeadRow {
    head_seq: u64,
}

#[derive(Debug, Row, Deserialize)]
struct LeafRow {
    event_hash: [u8; 32],
}

#[derive(Debug, Row, Deserialize)]
struct ListingRow {
    tenant_id: String,
    system_id: String,
    head_seq: u64,
    event_count: u64,
}

/// The scalar half of a summary, in one pass over the chain's rows.
#[derive(Debug, Row, Deserialize)]
struct SummaryRow {
    head_seq: u64,
    event_count: u64,
    /// Microseconds. ClickHouse returns 0 for an empty set rather than NULL,
    /// so an empty chain is distinguished by `event_count` and not by these.
    first_event_at: i64,
    last_event_at: i64,
    generations: u64,
    events_with_gaps: u64,
}

/// One `GROUP BY` result. Reused for every counted dimension.
#[derive(Debug, Row, Deserialize)]
struct CountRow {
    name: String,
    count: u64,
}

pub struct ClickHouseChains {
    client: clickhouse::Client,
}

impl std::fmt::Debug for ClickHouseChains {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClickHouseChains").finish_non_exhaustive()
    }
}

impl ClickHouseChains {
    /// `url` is the HTTP endpoint, typically port 8123.
    #[must_use]
    pub fn new(url: &str, database: &str) -> Self {
        Self {
            client: clickhouse::Client::default()
                .with_url(url)
                .with_database(database),
        }
    }

    #[must_use]
    pub fn with_credentials(mut self, user: &str, password: &str) -> Self {
        self.client = self.client.with_user(user).with_password(password);
        self
    }
}

fn source_err(e: &clickhouse::error::Error) -> ControlError {
    ControlError::Db(e.to_string())
}

impl ChainSource for ClickHouseChains {
    /// Every chain with at least one event, in a stable order so two
    /// control-plane replicas walk them in the same sequence.
    ///
    /// A full-table `DISTINCT` looks alarming and is not: `(tenant_id,
    /// system_id, seq)` is the table's sort key, so this reads the sparse
    /// index rather than the rows.
    async fn chains(&self) -> Result<Vec<ChainId>, ControlError> {
        let rows = self
            .client
            .query(
                "SELECT DISTINCT tenant_id, system_id FROM audit_events \
                 ORDER BY tenant_id, system_id",
            )
            .fetch_all::<ChainRow>()
            .await
            .map_err(|e| source_err(&e))?;

        Ok(rows
            .into_iter()
            .map(|r| ChainId {
                tenant_id: r.tenant_id,
                system_id: r.system_id,
            })
            .collect())
    }

    async fn head_seq(&self, chain: &ChainId) -> Result<Option<u64>, ControlError> {
        let rows = self
            .client
            .query(
                "SELECT max(seq) AS head_seq FROM audit_events \
                 WHERE tenant_id = ? AND system_id = ?",
            )
            .bind(&chain.tenant_id)
            .bind(&chain.system_id)
            .fetch_all::<HeadRow>()
            .await
            .map_err(|e| source_err(&e))?;

        // An empty chain aggregates to 0 rather than to no rows, and seq is
        // 1-based, so 0 means "nothing here" and not "one event".
        Ok(rows
            .into_iter()
            .next()
            .map(|h| h.head_seq)
            .filter(|&seq| seq > 0))
    }

    /// The `ORDER BY` is the whole contract — see the module docs.
    ///
    /// The count is checked by the caller, not here, and that check is what
    /// catches a duplicated `seq`: a redelivery older than the ingester's
    /// dedupe window (D10) puts two rows at one `seq`, and the range then
    /// reads long. Signing it anyway would attest a root over a different set
    /// of events than the body claims — a valid signature over a false
    /// statement, which is worse than no checkpoint.
    ///
    /// Holds every hash in the range resident, which is D1 seen from the
    /// writing side: 32 bytes per event, so a 10M-event catch-up is ~320MB.
    async fn leaves(
        &self,
        chain: &ChainId,
        seq_from: u64,
        seq_to: u64,
    ) -> Result<Vec<Hash32>, ControlError> {
        let rows = self
            .client
            .query(
                "SELECT event_hash FROM audit_events \
                 WHERE tenant_id = ? AND system_id = ? AND seq BETWEEN ? AND ? \
                 ORDER BY seq",
            )
            .bind(&chain.tenant_id)
            .bind(&chain.system_id)
            .bind(seq_from)
            .bind(seq_to)
            .fetch_all::<LeafRow>()
            .await
            .map_err(|e| source_err(&e))?;

        Ok(rows
            .into_iter()
            .map(|r| Hash32::from_bytes(r.event_hash))
            .collect())
    }
}

impl ChainExport for ClickHouseChains {
    /// Whole events, in `seq` order, for the auditor's copy of the chain.
    ///
    /// The row mapping is **the ingester's**, imported rather than rewritten.
    /// It is hash-critical — a conversion that changes one hashed byte
    /// produces an export that fails to verify while the stored chain is
    /// perfectly fine — and `round_trips_without_changing_the_event_hash` over
    /// there is the test that owns it. A second copy of that mapping here
    /// would be a second thing to keep correct, and the failure would look
    /// like a broken chain rather than a broken exporter.
    async fn events(
        &self,
        chain: &ChainId,
        seq_from: u64,
        seq_to: u64,
    ) -> Result<Vec<AuditEvent>, ControlError> {
        let rows = self
            .client
            .query(
                "SELECT ?fields FROM audit_events \
                 WHERE tenant_id = ? AND system_id = ? AND seq BETWEEN ? AND ? \
                 ORDER BY seq",
            )
            .bind(&chain.tenant_id)
            .bind(&chain.system_id)
            .bind(seq_from)
            .bind(seq_to)
            .fetch_all::<AuditEventRow>()
            .await
            .map_err(|e| source_err(&e))?;

        rows.into_iter()
            .map(|r| {
                AuditEvent::try_from(r).map_err(|e| {
                    // Refused, not skipped. A row this build cannot read is a
                    // hole in the evidence, and an export that quietly steps
                    // over one hands an auditor a chain with a gap that
                    // verifies — the worst possible outcome.
                    ControlError::Db(format!("chain {chain}: {e}"))
                })
            })
            .collect()
    }
}

impl crate::overview::ChainOverview for ClickHouseChains {
    /// Every chain, with its head and its event count, in one query.
    ///
    /// `max(seq)` and `count()` rather than two round trips per chain: a
    /// deployment with a thousand systems would otherwise make a thousand
    /// queries to paint one list. Both aggregates read the sort key
    /// `(tenant_id, system_id, seq)`, so this is an index scan.
    async fn listings(&self) -> Result<Vec<crate::overview::ChainListing>, ControlError> {
        let rows = self
            .client
            .query(
                "SELECT tenant_id, system_id, max(seq) AS head_seq, count() AS event_count \
                   FROM audit_events \
                  GROUP BY tenant_id, system_id \
                  ORDER BY tenant_id, system_id",
            )
            .fetch_all::<ListingRow>()
            .await
            .map_err(|e| source_err(&e))?;

        Ok(rows
            .into_iter()
            .map(|r| crate::overview::ChainListing {
                tenant_id: r.tenant_id,
                system_id: r.system_id,
                head_seq: r.head_seq,
                event_count: r.event_count,
            })
            .collect())
    }

    /// The counted shape of one chain.
    ///
    /// Five queries rather than one: ClickHouse has no clean way to return
    /// several independent `GROUP BY` results in a single response, and the
    /// alternative — one query per dimension with `arrayJoin` gymnastics —
    /// trades legibility for a round trip that is not on anybody's latency
    /// budget. All five are filtered on the leading two columns of the sort
    /// key.
    async fn summary(
        &self,
        chain: &ChainId,
    ) -> Result<crate::overview::ChainSummary, ControlError> {
        // `has_gap()` in SQL. It has to agree with `Pins::has_gap` in
        // `ancre-types`, and the two are enforced to agree by a test that runs
        // this query against a seeded row — a summary that under-counts gaps
        // is a dashboard that under-reports exactly what it exists to report.
        const GAP: &str = "(model_version = 'unknown' OR startsWith(model_version, 'unresolved:') \
             OR system_version = 'unknown' OR ifu_version = 'unknown' \
             OR model_id = 'unknown' OR prompt_id = 'unknown' \
             OR prompt_version = 'unknown' OR policy_id = 'unknown' \
             OR policy_version = 'unknown' OR gateway_version = 'unknown')";

        let scalars = self
            .client
            .query(&format!(
                "SELECT max(seq) AS head_seq, \
                        count() AS event_count, \
                        toInt64(min(toUnixTimestamp64Micro(occurred_at))) AS first_event_at, \
                        toInt64(max(toUnixTimestamp64Micro(occurred_at))) AS last_event_at, \
                        uniqExact(config_generation) AS generations, \
                        countIf({GAP}) AS events_with_gaps \
                   FROM audit_events WHERE tenant_id = ? AND system_id = ?"
            ))
            .bind(&chain.tenant_id)
            .bind(&chain.system_id)
            .fetch_all::<SummaryRow>()
            .await
            .map_err(|e| source_err(&e))?;

        let scalars = scalars.into_iter().next().unwrap_or(SummaryRow {
            head_seq: 0,
            event_count: 0,
            first_event_at: 0,
            last_event_at: 0,
            generations: 0,
            events_with_gaps: 0,
        });

        let risk_flags = self
            .counts(
                chain,
                "SELECT arrayJoin(risk_flags) AS name, count() AS count FROM audit_events \
                  WHERE tenant_id = ? AND system_id = ? \
                  GROUP BY name ORDER BY count DESC, name",
            )
            .await?;
        let event_types = self
            .counts(
                chain,
                "SELECT toString(event_type) AS name, count() AS count FROM audit_events \
                  WHERE tenant_id = ? AND system_id = ? \
                  GROUP BY name ORDER BY count DESC, name",
            )
            .await?;
        let outcomes = self
            .counts(
                chain,
                "SELECT toString(outcome) AS name, count() AS count FROM audit_events \
                  WHERE tenant_id = ? AND system_id = ? \
                  GROUP BY name ORDER BY count DESC, name",
            )
            .await?;
        let model_versions = self
            .counts(
                chain,
                "SELECT toString(model_version) AS name, count() AS count FROM audit_events \
                  WHERE tenant_id = ? AND system_id = ? \
                  GROUP BY name ORDER BY count DESC, name",
            )
            .await?;

        Ok(crate::overview::ChainSummary {
            tenant_id: chain.tenant_id.clone(),
            system_id: chain.system_id.clone(),
            head_seq: scalars.head_seq,
            event_count: scalars.event_count,
            // Zero from an empty set is not a timestamp. Reported as absent
            // rather than as the epoch, because a dashboard rendering 1970 for
            // an empty chain reads as a bug in the chain rather than in the
            // chart.
            first_event_at: (scalars.event_count > 0).then_some(scalars.first_event_at),
            last_event_at: (scalars.event_count > 0).then_some(scalars.last_event_at),
            risk_flags,
            event_types,
            outcomes,
            generations: scalars.generations,
            model_versions,
            events_with_gaps: scalars.events_with_gaps,
        })
    }
}

impl ClickHouseChains {
    /// One `GROUP BY` over a chain, as name/count pairs.
    async fn counts(
        &self,
        chain: &ChainId,
        sql: &str,
    ) -> Result<Vec<crate::overview::FlagCount>, ControlError> {
        let rows = self
            .client
            .query(sql)
            .bind(&chain.tenant_id)
            .bind(&chain.system_id)
            .fetch_all::<CountRow>()
            .await
            .map_err(|e| source_err(&e))?;

        Ok(rows
            .into_iter()
            .map(|r| crate::overview::FlagCount {
                name: r.name,
                count: r.count,
            })
            .collect())
    }
}
