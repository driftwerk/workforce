-- Shadow mode's weekly verdicts (DW-676, PR 9c): each store's ISO week,
-- judged from its raw comparisons while they are inside their 14 days,
-- and recorded here before the leader's retention pass deletes them. A
-- week's p99 cannot be worked out again once they are gone, so the pass
-- does not delete a week's comparisons until its verdict is written.
-- Kept 180 days by the leader's retention pass.
--
-- V033: V032 is the known-item runs (PR 10). Refinery refuses a lower
-- version found after a higher one was applied, so this merges after V032.

CREATE TABLE IF NOT EXISTS shadow_verdicts (
    store           TEXT NOT NULL,
    -- The week's first UTC day (a Monday) and its last.
    week_start      DATE NOT NULL,
    week_end        DATE NOT NULL,
    -- pass, fail, too_few (too few to judge) or not_set (the agreement
    -- thresholds were not set).
    verdict         TEXT NOT NULL
        CHECK (verdict IN ('pass', 'fail', 'too_few', 'not_set')),
    -- The whole report the verdict was worked out in: each gate, each day.
    report          JSONB NOT NULL,
    recorded_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (store, week_start)
);
