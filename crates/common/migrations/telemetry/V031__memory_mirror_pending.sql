-- Shadow mode for memories (DW-676, PR 7b): the memories written through
-- the hub since vellev's replica last took them, for the leader's applier
-- to bring the replica in line with Weaviate one memory at a time, soon
-- after each write rather than at the next reconcile.
--
-- One row a memory. Each hub's noter upserts a row after Weaviate took a
-- write, off the request path; each noting takes a new noted_seq. The
-- applier takes a row away only at the noted_seq it read before applying,
-- so a noting made meanwhile stays to be applied; one it could not apply
-- is noted afresh, under a new noted_seq, with why and when to try again.
--
-- Convergence rests on the reconcile (V030), not on this list: a note
-- dropped (a full channel, a hub stopped before writing it, a write that
-- does not go through the hub) is found by the next reconcile. This list
-- is for lag alone. Rows go when applied, so nothing prunes it; when the
-- shadow is abandoned it is dropped with the rest.
--
-- V031: after V030 (the reconcile's tables). Refinery refuses a lower
-- version found after a higher one was applied.

CREATE SEQUENCE IF NOT EXISTS memory_mirror_noted_seq;

CREATE TABLE IF NOT EXISTS memory_mirror_pending (
    memory_id        TEXT PRIMARY KEY,
    noted_seq        BIGINT NOT NULL DEFAULT nextval('memory_mirror_noted_seq'),
    noted_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Tries since it was last noted by a write.
    tries            INTEGER NOT NULL DEFAULT 0,
    last_error       TEXT,
    first_failed_at  TIMESTAMPTZ,
    next_try_at      TIMESTAMPTZ
);

CREATE INDEX IF NOT EXISTS memory_mirror_pending_due
    ON memory_mirror_pending (next_try_at NULLS FIRST);
