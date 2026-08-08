-- The control plane's registry. mvp-plan §3.
--
-- Not frozen the way `audit_events` is: nothing in here is hashed into a
-- chain, so a column can be added without changing what a past event means.
-- What *is* load-bearing is the ordering — every read behind `Registry` sorts,
-- because two control-plane replicas holding identical rows must build byte-
-- identical snapshots (resolver spec test 6). An unordered read makes that
-- fail intermittently, on one node, under load.
--
-- Checkpoints live here rather than in ClickHouse on purpose: they are the
-- attestation over the event store, and storing them in the store they attest
-- to hands anyone who can rewrite the events the ability to re-sign them too.

-- A system carries **no tenant_id**, and that is deliberate rather than an
-- omission. The snapshot's systems are keyed by `system_id` alone
-- (`ConfigSnapshot::build` refuses a duplicate), and the tenant a system
-- belongs to is knowable only through the keys bound to it — exactly as
-- `SystemConfig` carries no tenant. A tenant column here could disagree with
-- the binding in `api_keys`, and a chain is `(tenant_id, system_id)`, so the
-- disagreement would surface as events written into the wrong chain.
CREATE TABLE IF NOT EXISTS systems (
  system_id      text PRIMARY KEY,
  system_version text NOT NULL,
  ifu_version    text NOT NULL,
  -- The frozen wire form, matching `RiskClass::as_str`. Text and not an enum:
  -- the discriminants are pinned in the ClickHouse DDL, and having two
  -- databases own the same numbering is how they drift apart.
  risk_class     text NOT NULL
                 CHECK (risk_class IN ('unclassified','minimal','transparency','high')),
  -- 'none' is a fact, not a gap: no policy engine is configured in the MVP
  -- (PRD §16.2). It needs no risk flag and is still countable in a GROUP BY.
  policy_id      text NOT NULL DEFAULT 'none',
  policy_version text NOT NULL DEFAULT 'none',
  -- An index into the system's routes **in position order**, not a position
  -- value. The two coincide until a route is deleted, and no CHECK can say so
  -- from here — `ConfigSnapshot::build` catches an out-of-bounds default at
  -- publish time and the publish is refused, which is one alert rather than a
  -- fleet-wide cold resolver.
  default_route  integer NOT NULL DEFAULT 0 CHECK (default_route >= 0),
  archived       boolean NOT NULL DEFAULT false
);

-- Routes are first-match-wins, so `position` is data and not presentation.
-- It is the one collection `SnapshotSpec::canonicalize_order` does not sort.
CREATE TABLE IF NOT EXISTS routes (
  system_id      text    NOT NULL REFERENCES systems(system_id) ON DELETE CASCADE,
  position       integer NOT NULL,
  -- `Matcher` is a recursive enum; jsonb is the honest storage for it. Read
  -- back through serde, and a shape this build does not recognise is refused
  -- rather than defaulted to `Any` — a route that silently matches everything
  -- would repoint traffic without changing a pin anyone can see.
  matcher        jsonb   NOT NULL,
  model_id       text    NOT NULL,
  -- The provider's own pinned identifier, never a floating alias.
  model_version  text    NOT NULL,
  prompt_id      text    NOT NULL,
  prompt_version text    NOT NULL,
  prompt_hash    bytea   NOT NULL CHECK (octet_length(prompt_hash) = 32),
  PRIMARY KEY (system_id, position)
);

-- Prompt bodies, addressed by content hash. `GET /v1/prompts/{hash}` serves
-- these; the body is verified against its own key on the way out, because a
-- body that no longer hashes to the `prompt_version` an event pinned is not
-- the prompt that event used.
CREATE TABLE IF NOT EXISTS prompts (
  prompt_hash bytea PRIMARY KEY CHECK (octet_length(prompt_hash) = 32),
  body        bytea NOT NULL
);

-- The hash of an API key, never the key — a registry dump must not be a list of
-- working credentials.
--
-- It is the *workspace hash primitive*: BLAKE3 by default, SHA-256 under the
-- `hash-sha256` feature. Not `sha256sum` from a shell, and the two do not
-- agree, so a hand-computed hash produces a key that authenticates nothing.
-- `cargo run -p ancre-gateway --example key-hash -- <key>` calls the same
-- function the gateway calls, which is the only way to be sure it matches.
CREATE TABLE IF NOT EXISTS api_keys (
  key_hash   bytea PRIMARY KEY CHECK (octet_length(key_hash) = 32),
  tenant_id  text  NOT NULL,
  system_id  text  NOT NULL REFERENCES systems(system_id),
  revoked_at timestamptz
);

-- One row per published generation. Rows are never deleted: a `config_generation`
-- pin in a two-year-old event has to remain resolvable to the content it named.
--
-- Gaps are expected. A generation allocated for a publish whose bus send then
-- failed is burned rather than reused — two configurations sharing a generation
-- number would make every pin carrying it ambiguous forever.
CREATE TABLE IF NOT EXISTS snapshot_generations (
  generation   bigint PRIMARY KEY,
  content_hash bytea  NOT NULL CHECK (octet_length(content_hash) = 32),
  built_at     timestamptz NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS checkpoints (
  tenant_id     text        NOT NULL,
  system_id     text        NOT NULL,
  seq_from      bigint      NOT NULL,
  seq_to        bigint      NOT NULL,
  root_hash     bytea       NOT NULL CHECK (octet_length(root_hash) = 32),
  -- Signed, so it must round-trip to the microsecond. timestamptz is
  -- microsecond-resolution, which is exactly what `Timestamp` holds.
  built_at      timestamptz NOT NULL,
  canon_version text        NOT NULL,
  key_id        text        NOT NULL,
  signature     bytea       NOT NULL CHECK (octet_length(signature) = 64),
  PRIMARY KEY (tenant_id, system_id, seq_to)
);

-- Public halves only. The private key is a control-plane-local file under
-- age/SOPS in the MVP, moving to Vault for Enterprise (mvp-plan §8.3). A
-- database holding both the events' attestation key and the events is one
-- where a single compromised credential rewrites history and re-signs it.
--
-- Every key that was ever valid stays here with its window: rotation must not
-- invalidate old checkpoints, and an auditor verifying a two-year-old range
-- needs the key that signed it, not the current one.
CREATE TABLE IF NOT EXISTS signing_keys (
  key_id     text  PRIMARY KEY,
  public_key bytea NOT NULL CHECK (octet_length(public_key) = 32),
  valid_from timestamptz NOT NULL,
  valid_to   timestamptz          -- NULL for the active key
);

-- At most one active key. Without this, two control planes started with
-- different key files would both claim an open window, and a checkpoint could
-- not be attributed to a key by its timestamp alone.
CREATE UNIQUE INDEX IF NOT EXISTS signing_keys_one_active
  ON signing_keys ((valid_to IS NULL)) WHERE valid_to IS NULL;
