-- The durable dedupe index, for a table that already exists.
--
-- 001 carries the same index inline, so on a fresh volume this file is a
-- no-op and exists only to be idempotent. It is here for the other case: a
-- deployment created before M7, whose table the `CREATE TABLE IF NOT EXISTS`
-- in 001 will decline to touch. The ClickHouse image only runs these scripts
-- on first initialisation of the data volume, so an existing deployment has
-- to run this one by hand:
--
--   docker compose exec clickhouse clickhouse-client --queries-file \
--     /docker-entrypoint-initdb.d/002_dedupe_index.sql
--
-- Without it nothing is wrong and nothing is unsafe — the ingester's dedupe
-- lookup still returns the right answer. It just reads the chain's granules
-- instead of skipping them.

ALTER TABLE ancre.audit_events
  ADD INDEX IF NOT EXISTS idx_event_id event_id TYPE bloom_filter(0.01) GRANULARITY 1;

-- ADD INDEX only applies to parts written after it. Existing parts need this,
-- and it is a background mutation: it returns immediately and the index fills
-- in behind it.
ALTER TABLE ancre.audit_events MATERIALIZE INDEX idx_event_id;
