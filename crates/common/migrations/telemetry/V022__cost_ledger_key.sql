-- A durable coding round records its cost when its job finishes, before the
-- turn journals it. A hub that dies in between is replaced by a replica that
-- collects the same finished job and records it again. The job id keys the
-- row, so the second record is ignored; rows without a key are unaffected.
ALTER TABLE cost_records ADD COLUMN IF NOT EXISTS ledger_key TEXT;
CREATE UNIQUE INDEX IF NOT EXISTS idx_cost_ledger_key
    ON cost_records (ledger_key)
    WHERE ledger_key IS NOT NULL;
