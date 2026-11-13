-- What was sent of an item sent again when it changes (an account, a need,
-- a decision): its body's hash, so the sync sends only what changed (Sees
-- K3). NULL for one sent once (board activity, a transcript).
ALTER TABLE knows_sent ADD COLUMN hash TEXT;

-- Sessions the person forgot (Sees §8.3): nothing of them is sent again,
-- whatever of them is left behind (a boss's need or decision about one).
CREATE TABLE knows_forgotten (
    session UUID        PRIMARY KEY,
    at      TIMESTAMPTZ NOT NULL
);
