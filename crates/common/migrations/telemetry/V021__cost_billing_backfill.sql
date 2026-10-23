-- Rows written before V020 carry no origin; the ones a subscription paid for
-- are recognised by their ledger key's route prefix (Bedrock-billed Claude
-- Code rounds are metered).
UPDATE cost_records SET billing = 'subscription'
 WHERE (model LIKE 'claude-code:%' AND model NOT LIKE 'claude-code:bedrock:%')
    OR model LIKE 'codex:%' OR model LIKE 'claude-pilot%';
CREATE INDEX IF NOT EXISTS idx_cost_billing_ts ON cost_records(billing, timestamp);
