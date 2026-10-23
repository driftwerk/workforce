-- The transitional read of pre-V006 `task_active` markers is gone, so the
-- index that served it is dead weight. Session rows keep whatever marker keys
-- they still carry: nothing reads them, and rewriting every row to strip inert
-- JSON keys is not worth a migration.
DROP INDEX IF EXISTS sessions_legacy_turn_marker;
