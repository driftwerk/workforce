-- The models a person's own route runs, by kind of work, as JSON in an
-- agent's own words: {"model_preferences": {"conversation": ...},
-- "light_model": ..., "heavy_model": ...}. NULL is the route's defaults.
--
-- Nullable columns only: no rewrite, no lock beyond the catalogue's.

ALTER TABLE user_settings ADD COLUMN IF NOT EXISTS llm_models TEXT;
ALTER TABLE user_agent_settings ADD COLUMN IF NOT EXISTS llm_models TEXT;
