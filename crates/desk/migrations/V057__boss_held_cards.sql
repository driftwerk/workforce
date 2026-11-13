-- A boss's needs-you held while the person types in its DM (overseer H8):
-- kept until it goes out, so a restart inside the hold still posts it, at
-- the latest by `post_by` (three minutes from when the typing began).
CREATE TABLE boss_held_cards (
    id        UUID        PRIMARY KEY,
    boss      TEXT        NOT NULL,
    -- The open item it puts to the person; posted only while that is open.
    item_id   UUID,
    text      TEXT        NOT NULL,
    extras    JSONB       NOT NULL,
    post_by   TIMESTAMPTZ NOT NULL,
    -- Taken to post until then: a post that fails or dies with the hub
    -- leaves it to be taken again after.
    claimed_until TIMESTAMPTZ
);
CREATE INDEX boss_held_cards_post_by ON boss_held_cards (post_by);
