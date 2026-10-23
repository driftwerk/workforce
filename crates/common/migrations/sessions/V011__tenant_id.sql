-- Tenant column on every table that holds a tenant's data. The default keeps
-- a deployment without tenants (the hub) exactly as it was: every row it has
-- written, and every row it writes from here on, is 'default'. The check
-- keeps a row from ever carrying the empty string, which is what
-- `current_setting('app.tenant_id', true)` reads once a transaction that set
-- it has ended — a policy comparing to it must never find a match.
-- `instance_heartbeats` is fleet liveness, not tenant data, and stays as it
-- is.
--
-- This migration is ADDITIVE, in two steps with the edge's own set:
--
--   1. Here, in the set the hub and the edge share, the tenant-first indexes
--      are created under NEW names and every index and primary key from
--      before is left in place. The hub rolls out replica by replica, and a
--      replica on the previous binary names the old unique targets in its
--      `ON CONFLICT` clauses (`sessions_active_uniq`, `turn_claims_pkey`,
--      `session_archive_message_uniq`); dropping them here would fail that
--      replica with 42P10 until the roll-out completed. The hub is
--      single-tenant, so the old and the new invariants agree there and
--      both can hold.
--
--   2. The edge's set (`crates/edge/migrations/edge/V002__tenants.sql`), which
--      runs on the edge's database only, drops the old unique indexes and
--      replaces the primary keys with tenant-first ones, so two tenants may
--      hold the same (agent, channel, owner).
--
-- While both shapes exist the session store's `ON CONFLICT` clauses name
-- neither: a clause that names one index makes it the sole arbiter, and a
-- race that lands on the other is then a unique violation rather than a
-- no-op. With no target every unique index arbitrates, and the only others
-- are a fresh UUID and a sequence. The turn-claim upsert is `DO UPDATE`,
-- which needs one arbiter, so it names the primary key by its name
-- (`turn_claims_pkey`) and `turn_claims` gets NO second unique index here:
-- the edge's set rebuilds the primary key itself, under the same name.

ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS tenant_id TEXT NOT NULL DEFAULT 'default'
        CHECK (tenant_id <> '');
ALTER TABLE session_archive
    ADD COLUMN IF NOT EXISTS tenant_id TEXT NOT NULL DEFAULT 'default'
        CHECK (tenant_id <> '');
ALTER TABLE turn_claims
    ADD COLUMN IF NOT EXISTS tenant_id TEXT NOT NULL DEFAULT 'default'
        CHECK (tenant_id <> '');
ALTER TABLE turn_journal
    ADD COLUMN IF NOT EXISTS tenant_id TEXT NOT NULL DEFAULT 'default'
        CHECK (tenant_id <> '');

-- One active (unsealed) session per conversation, per tenant, which
-- `get_or_create`'s insert converges on. `sessions_active_uniq` (without the
-- tenant) stays beside it; see above.
CREATE UNIQUE INDEX sessions_tenant_active_uniq
    ON sessions (tenant_id, agent_id, channel, owner)
    WHERE NOT sealed;

CREATE INDEX sessions_tenant_agent_updated
    ON sessions (tenant_id, agent_id, updated_at DESC);

CREATE INDEX sessions_tenant_owner_updated
    ON sessions (tenant_id, agent_id, owner, updated_at DESC);

-- One archive row per (tenant, session, message), which the archive append
-- converges on; `session_archive_message_uniq` stays beside it.
CREATE UNIQUE INDEX session_archive_tenant_message_uniq
    ON session_archive (tenant_id, session_id, message_id);

-- The journal's reads; its primary key, whichever shape it has, arbitrates
-- the append.
CREATE INDEX turn_journal_tenant_session
    ON turn_journal (tenant_id, agent_id, session_id, seq);
