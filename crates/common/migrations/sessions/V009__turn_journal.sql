-- The journal of a turn in flight: each tool call's result as it returns, and
-- the plan of work a call left running, so a turn that dies with its hub can
-- be taken up where it stopped instead of being rerun.
--
-- It sits beside `sessions` rather than in it: appending never rewrites the
-- transcript or bumps its revision, so a turn can journal as it goes without
-- making its own end-of-turn save, or anyone else's, lose a conflict.
CREATE TABLE turn_journal (
    agent_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    call_id TEXT NOT NULL,
    entry_key TEXT NOT NULL,
    seq BIGSERIAL,
    entry JSONB NOT NULL,
    PRIMARY KEY (agent_id, session_id, call_id, entry_key)
);
CREATE INDEX turn_journal_session ON turn_journal (agent_id, session_id, seq);
