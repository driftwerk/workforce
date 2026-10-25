-- Shadow mode for memories (DW-676, PR 11): what has ever been done to the
-- store memories switched from (Weaviate's WorkforceMemory), kept for good.
--
-- 'promoted': memories were promoted to vellev. 'previous_left': a hub
-- opened memories with [embeddings] keep_previous = false. After either,
-- that store has missed erasures: nothing of the hub's reached it. A hub
-- that kept it again would catch an erased memory back up from it, so the
-- memory guard refuses keep_previous = true once a row is here. Neither
-- a rollback nor anything else deletes these rows.
--
-- V034: after V032 (PR 10, known-item runs) and V033 (PR 9c, verdicts).
-- Refinery refuses a lower version found after a higher one was applied,
-- so this merges after both.

CREATE TABLE IF NOT EXISTS shadow_memory_history (
    event     TEXT PRIMARY KEY CHECK (event IN ('promoted', 'previous_left')),
    first_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_at   TIMESTAMPTZ NOT NULL DEFAULT now()
);
