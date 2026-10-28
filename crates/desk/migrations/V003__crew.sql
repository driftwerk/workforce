-- What happened on the crew: agents' turns, sessions' states, the overseer's
-- and the person's actions. Always recorded, newest read first.
CREATE TABLE activity (
    id            TEXT        PRIMARY KEY,
    at            TIMESTAMPTZ NOT NULL,
    kind          TEXT        NOT NULL,
    actor         JSONB       NOT NULL,
    subject       JSONB,
    text          TEXT        NOT NULL,
    reason        TEXT,
    suggestion_id TEXT,
    approval_id   TEXT,
    hop           SMALLINT    NOT NULL DEFAULT 0,
    detail        JSONB
);

CREATE INDEX activity_timeline ON activity (at DESC, id DESC);
CREATE INDEX activity_subject ON activity ((subject ->> 'id'), at DESC);

-- What the overseer proposed instead of doing it.
CREATE TABLE suggestions (
    id         TEXT        PRIMARY KEY,
    at         TIMESTAMPTZ NOT NULL,
    agent_id   TEXT        NOT NULL,
    text       TEXT        NOT NULL,
    action     JSONB       NOT NULL,
    state      TEXT        NOT NULL,
    subject    JSONB,
    updated_at TIMESTAMPTZ NOT NULL
);

CREATE INDEX suggestions_recent ON suggestions (at DESC);

-- What an overseer's DM message mentions to act on in place, by message.
CREATE TABLE message_extras (
    message_id TEXT        PRIMARY KEY,
    agent_id   TEXT        NOT NULL,
    extras     JSONB       NOT NULL,
    at         TIMESTAMPTZ NOT NULL
);

-- The overseer's unprompted turns: wake-ups and summaries, and their replies.
CREATE TABLE check_ins (
    id       TEXT        PRIMARY KEY,
    at       TIMESTAMPTZ NOT NULL,
    agent_id TEXT        NOT NULL,
    kind     TEXT        NOT NULL,
    prompt   TEXT        NOT NULL,
    reply    TEXT
);

CREATE INDEX check_ins_recent ON check_ins (at DESC);
