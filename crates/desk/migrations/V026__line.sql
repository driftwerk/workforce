-- What sessions, the overseer and the person said to each other on Desk's
-- lines. A row is delivered to its recipient once, when it can take it.
-- `sender` and `recipient` are Desk parties (a session, the overseer agent,
-- or `you`), as Activity keeps them.
CREATE TABLE line_messages (
    id           UUID        PRIMARY KEY,
    at           TIMESTAMPTZ NOT NULL,
    sender       JSONB       NOT NULL,
    recipient    JSONB       NOT NULL,
    kind         TEXT        NOT NULL,   -- 'ask' | 'tell' | 'answer'
    text         TEXT        NOT NULL,
    in_reply_to  UUID        REFERENCES line_messages (id) ON DELETE SET NULL,
    hop          SMALLINT    NOT NULL,
    delivery     TEXT        NOT NULL,   -- 'queued' | 'handed' | 'delivered' | 'held' | 'refused'
    asked        TEXT,                   -- an ask's: 'open' | 'escalated' | 'answered' | 'withdrawn'
    escalation   JSONB,
    -- The sender's sidebar group when it was sent.
    group_id     TEXT,
    handed_at    TIMESTAMPTZ,
    delivered_at TIMESTAMPTZ,
    updated_at   TIMESTAMPTZ NOT NULL
);
CREATE INDEX line_inbox ON line_messages ((recipient ->> 'id'), at)
    WHERE delivery IN ('queued', 'handed', 'held');
CREATE INDEX line_thread_to ON line_messages ((recipient ->> 'id'), at DESC);
CREATE INDEX line_thread_from ON line_messages ((sender ->> 'id'), at DESC);
CREATE INDEX line_asks ON line_messages (at) WHERE asked IN ('open', 'escalated');
