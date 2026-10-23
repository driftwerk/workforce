-- Cache-aware cost tracking. Cache reads + writes are billed separately
-- (Anthropic: read=0.10x input, write=1.25x input; Gemini: read=0.25x input).
-- Old rows default to 0, which makes calculate_cost fall back to the
-- previous flat-input behaviour.
ALTER TABLE cost_records
    ADD COLUMN IF NOT EXISTS cache_creation_input_tokens BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN IF NOT EXISTS cache_read_input_tokens BIGINT NOT NULL DEFAULT 0;
