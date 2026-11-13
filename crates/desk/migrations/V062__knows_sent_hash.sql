-- What was sent of an item sent again when it changes (an account, a need,
-- a decision): its body's hash, so the sync sends only what changed (Sees
-- K3). NULL for one sent once (board activity, a transcript).
ALTER TABLE knows_sent ADD COLUMN hash TEXT;
