-- The audit_events schema. mvp-plan §4.
--
-- FROZEN IN WEEK 1. Adding a hashed column later changes the canonical
-- encoding, which means every chain written before the change verifies under a
-- different rule set — you would have to re-key or maintain a rule-set
-- registry forever. canon_version exists so that if you must break it, you
-- break it explicitly and loudly.
--
-- Columns V1 needs (payload_ref, subject_key_id) exist now, empty, so that V1
-- does not bump canon_version.
--
-- Note: ClickHouse has ALTER DELETE. Append-only is enforced by the hash
-- chain, not by the engine. That is the whole point of the chain.

-- Explicitly qualified. The image's entrypoint runs these scripts against
-- `default` regardless of CLICKHOUSE_DB, so an unqualified CREATE puts the
-- table in a database the ingester is not pointed at — and the failure is a
-- "table does not exist" on the first insert, ten minutes into a quickstart.
CREATE DATABASE IF NOT EXISTS ancre;

CREATE TABLE IF NOT EXISTS ancre.audit_events (
  -- identity
  tenant_id         LowCardinality(String),
  system_id         LowCardinality(String),
  seq               UInt64,          -- assigned by INGESTER, gapless per (tenant, system)
  event_id          UUID,
  trace_id          String,
  attempt_seq       UInt16,          -- failover: one event per attempt

  -- chain
  prev_hash         FixedString(32),
  event_hash        FixedString(32),
  canon_version     LowCardinality(String),

  -- time / origin
  occurred_at       DateTime64(6, 'UTC'),
  ingested_at       DateTime64(6, 'UTC'),
  node_id           LowCardinality(String),

  -- event
  event_type        LowCardinality(String),  -- llm.request | provider.failover
                                             -- | config.generation.applied
                                             -- | pin.overridden | telemetry.dropped
                                             -- | chain.heartbeat
  outcome           LowCardinality(String),  -- ok | error | denied | interrupted

  -- pins.  NEVER NULL.  Unknown is the literal string 'unknown' + a risk flag
  config_generation UInt64,
  config_hash       FixedString(32),
  system_version    LowCardinality(String),
  ifu_version       LowCardinality(String),
  model_id          LowCardinality(String),
  model_version     String,
  prompt_id         LowCardinality(String),
  prompt_version    String,
  policy_id         LowCardinality(String),
  policy_version    String,
  gateway_version   LowCardinality(String),
  risk_class        Enum8('unclassified'=0,'minimal'=1,'transparency'=2,'high'=3),
  resolved_stale    UInt8,
  risk_flags        Array(LowCardinality(String)),

  -- payload: digests now, crypto-shredded bodies in V1
  request_digest    FixedString(32),
  response_digest   FixedString(32),
  payload_ref       String,          -- '' in MVP
  subject_key_id    String,          -- '' in MVP

  -- metrics
  provider          LowCardinality(String),
  http_status       UInt16,
  latency_ms        UInt32,
  ttft_ms           UInt32,
  tokens_in         UInt32,
  tokens_out        UInt32,
  error_code        LowCardinality(String),

  -- Makes the ingester's durable dedupe lookup cheap. `event_id` is not in the
  -- sorting key and never can be — the sorting key is what makes a chain read
  -- back in seq order — so without a skip index, "do you already hold these
  -- ids" reads every granule of the chain's partition.
  --
  -- This is not a schema change in the sense that matters. The freeze is about
  -- hashed *columns*: adding one changes the canonical encoding and every
  -- chain written before it verifies under a different rule set. A skip index
  -- adds no column, changes no row, and leaves canon_version alone.
  INDEX idx_event_id event_id TYPE bloom_filter(0.01) GRANULARITY 1
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(occurred_at)
ORDER BY (tenant_id, system_id, seq)
-- 7-year default ceiling. The ≥180-day floor is enforced in the control plane,
-- where it can be made un-lowerable; a TTL here can be edited by anyone with
-- DDL rights.
TTL toDateTime(occurred_at) + INTERVAL 7 YEAR;
