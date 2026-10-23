-- What caused each LLM call (`origin`) and what paid for it (`billing`,
-- `payer`, `credential_tier`), so the ledger separates subscription work from
-- metered spend. Only the columns: with constant defaults these are
-- metadata-only, so the table is locked for no longer than the catalog
-- change. The backfill and index follow in V021.
ALTER TABLE cost_records ADD COLUMN IF NOT EXISTS origin TEXT;
ALTER TABLE cost_records ADD COLUMN IF NOT EXISTS billing TEXT NOT NULL DEFAULT 'metered';
ALTER TABLE cost_records ADD COLUMN IF NOT EXISTS payer TEXT;
ALTER TABLE cost_records ADD COLUMN IF NOT EXISTS credential_tier TEXT;
