-- Act fully's directives: the person's standing answers to a few questions
-- the boss asks when Act fully goes on ("Can I merge PRs?"). Each answer is
-- one of their preferences (boss_preferences), marked with the directive
-- it answers; the asking is kept per person and boss scope. Additive.
ALTER TABLE boss_preferences ADD COLUMN directive TEXT;
ALTER TABLE boss_preferences ADD COLUMN directive_answer TEXT;
-- One standing answer per directive of the starter set (their own each
-- stand).
CREATE UNIQUE INDEX boss_preferences_directive ON boss_preferences (owner, boss_scope, directive)
    WHERE directive IS NOT NULL AND directive <> 'own' AND state <> 'retired';
CREATE TABLE boss_directive_asks (
    owner        TEXT        NOT NULL,
    -- main | a sidebar group's id
    boss_scope   TEXT        NOT NULL,
    -- When the boss was last seen at Act fully, unbroken: cleared when it
    -- isn't, so Act fully turned on again asks again.
    full_since   TIMESTAMPTZ,
    -- The card it posted in its DM, and when.
    card_message TEXT,
    card_at      TIMESTAMPTZ,
    -- The last time an update said what's still open.
    nudged_at    TIMESTAMPTZ,
    -- "Skip for now" on the card.
    skipped_at   TIMESTAMPTZ,
    -- "Stop asking".
    stop_asking  BOOLEAN     NOT NULL DEFAULT false,
    PRIMARY KEY (owner, boss_scope)
);
