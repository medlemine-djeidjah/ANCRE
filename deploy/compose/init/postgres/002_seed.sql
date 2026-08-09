-- Seed data: one tenant, one system, enough to produce a chain worth reading.
--
-- A quickstart that starts with an empty registry is a quickstart that starts
-- with a 503 — the gateway installs the empty snapshot, refuses every key, and
-- the first thing a stranger sees is the product failing (docs/deferred.md,
-- D16). So the registry ships populated, and `docker compose up` has a
-- customer in it.
--
-- ## The hashes below are computed, not typed
--
-- Both are the *workspace hash primitive* — BLAKE3 by default, SHA-256 under
-- the `hash-sha256` feature — and `sha256sum` from a shell does not agree with
-- either. Regenerate them with the same function the gateway calls:
--
--   cargo run -p ancre-gateway --example key-hash -- ancre-demo-key
--   cargo run -p ancre-gateway --example key-hash -- "$(cat the prompt body)"
--
-- `crates/ancre-gateway/tests/seed.rs` asserts that what is written here still
-- matches what that function returns. A seed whose key hash has silently
-- drifted authenticates nothing, and the symptom is a 401 ten minutes into
-- somebody's first evaluation.
--
-- ## Idempotent
--
-- The Postgres entrypoint only runs this on an empty data directory, so
-- re-seeding a running deployment means piping this file into `psql` by hand.
-- Every statement below tolerates being re-run, which is what makes that safe:
--
--   docker compose exec -T postgres psql -U ancre -d ancre \
--     < deploy/compose/init/postgres/002_seed.sql

-- The prompt body, addressed by its own content hash. Stored so that
-- `GET /v1/prompts/{hash}` can show an auditor the text an event pinned —
-- the digest in the chain is only as useful as the ability to produce what it
-- is a digest of.
INSERT INTO prompts (prompt_hash, body) VALUES (
  decode('6a4913393d5480619887cbb83ed4d49296cdeb23b94aacfb4618fbb5597fd7a6', 'hex'),
  convert_to(
    'You are a screening assistant. Summarise the candidate''s CV against the job description. State only what the CV says, and never infer age, gender, ethnicity, health or any other protected characteristic.',
    'UTF8')
) ON CONFLICT (prompt_hash) DO NOTHING;

-- CV screening is Annex III point 4(a) — employment, recruitment, filtering of
-- applications — so `high` is the right class and is also the interesting one:
-- a High-risk system is the one the gateway will refuse to serve on stale
-- configuration (`ANCRE_FAIL_CLOSED_ON_STALE`).
INSERT INTO systems (system_id, system_version, ifu_version, risk_class,
                     policy_id, policy_version, default_route, archived)
VALUES ('hr-screening', '2.4.1', 'ifu-2026-03', 'high', 'none', 'none', 2, false)
ON CONFLICT (system_id) DO UPDATE SET
  system_version = EXCLUDED.system_version,
  ifu_version    = EXCLUDED.ifu_version,
  risk_class     = EXCLUDED.risk_class,
  default_route  = EXCLUDED.default_route,
  archived       = EXCLUDED.archived;

-- Three routes, and the order is the semantics: first match wins, so every
-- specific matcher has to come before `Any` or it can never fire. That is why
-- `default_route` above is 2 and not 0 — it indexes routes in position order,
-- and the catch-all is the last of the three.
--
-- The second route exists to make the *unhappy* pin visible. The registry
-- pins `gpt-4o-preview-2025-01-01`, the mock provider answers with the
-- floating alias `gpt-4o-preview`, and the resulting event carries
-- `model_version = unresolved:gpt-4o-preview` and the `unpinned_model` risk
-- flag. An evidence system that only ever demonstrates clean rows has not been
-- demonstrated to work.
INSERT INTO routes (system_id, position, matcher, model_id, model_version,
                    prompt_id, prompt_version, prompt_hash)
VALUES
  ('hr-screening', 0, '{"ModelAlias":"gpt-4o-preview"}'::jsonb,
   'gpt-4o-preview', 'gpt-4o-preview-2025-01-01',
   'cv-screen', 'b3:6a4913393d54',
   decode('6a4913393d5480619887cbb83ed4d49296cdeb23b94aacfb4618fbb5597fd7a6', 'hex')),
  -- A second provider behind the same OpenAI-wire ingress. The client changes
  -- one string in its request and nothing else: the gateway translates the
  -- body, rewrites the path to Anthropic's `/v1/messages`, and authenticates
  -- with the deployment's Anthropic key. That the route exists at all is the
  -- demonstration — a customer's multi-provider policy becomes a registry row.
  ('hr-screening', 1, '{"ModelAlias":"claude-sonnet-4-5"}'::jsonb,
   'claude-sonnet-4-5', 'claude-sonnet-4-5-20250929',
   'cv-screen', 'b3:6a4913393d54',
   decode('6a4913393d5480619887cbb83ed4d49296cdeb23b94aacfb4618fbb5597fd7a6', 'hex')),
  ('hr-screening', 2, '"Any"'::jsonb,
   'gpt-4o', 'gpt-4o-2024-08-06',
   'cv-screen', 'b3:6a4913393d54',
   decode('6a4913393d5480619887cbb83ed4d49296cdeb23b94aacfb4618fbb5597fd7a6', 'hex'))
ON CONFLICT (system_id, position) DO UPDATE SET
  matcher        = EXCLUDED.matcher,
  model_id       = EXCLUDED.model_id,
  model_version  = EXCLUDED.model_version,
  prompt_id      = EXCLUDED.prompt_id,
  prompt_version = EXCLUDED.prompt_version,
  prompt_hash    = EXCLUDED.prompt_hash;

-- The demo key is `ancre-demo-key`, and it is in a public repository, which is
-- the reason it is a demo key: it binds to one seeded tenant on a deployment
-- with a mock provider behind it. A real key is minted by the customer and its
-- hash is the only thing that ever reaches this table — a registry dump must
-- not be a list of working credentials.
INSERT INTO api_keys (key_hash, tenant_id, system_id, revoked_at) VALUES (
  decode('4fd6c775b6746ee042b45c52eb6416f84aeb763bb98eee5f4177021cbe728c25', 'hex'),
  'acme', 'hr-screening', NULL
) ON CONFLICT (key_hash) DO UPDATE SET
  tenant_id  = EXCLUDED.tenant_id,
  system_id  = EXCLUDED.system_id,
  revoked_at = NULL;
