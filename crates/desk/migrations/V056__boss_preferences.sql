-- The person's preferences for their boss (the boss understands sessions,
-- H9 and H10): exact, listable, editable and citable rules ("when a session
-- wants to merge with CI red, make it wait for green"), kept per person and
-- per boss scope (main, or a group's id) so they outlive whichever agent is
-- that scope's boss. A decision made by one names it (boss_decisions'
-- preference). Additive.
CREATE TABLE boss_preferences (
    id               UUID        PRIMARY KEY,
    owner            TEXT        NOT NULL DEFAULT '',
    -- main | a sidebar group's id
    boss_scope       TEXT        NOT NULL,
    when_text        TEXT        NOT NULL,
    then_text        TEXT        NOT NULL,
    -- all | group:<id> | repo:<owner/name>
    scope            TEXT        NOT NULL DEFAULT 'all',
    -- asked (an "always" answer) | said (said outright) | undone (learned
    -- from an Undo)
    source           TEXT        NOT NULL,
    -- The open item it came from, when one.
    from_item        UUID,
    created_at       TIMESTAMPTZ NOT NULL,
    changed_at       TIMESTAMPTZ NOT NULL,
    applied_count    INT         NOT NULL DEFAULT 0,
    last_applied_at  TIMESTAMPTZ,
    -- How often the person undid a decision made by it.
    overruled_count  INT         NOT NULL DEFAULT 0,
    -- active | paused | retired
    state            TEXT        NOT NULL DEFAULT 'active'
);
CREATE INDEX boss_preferences_scope ON boss_preferences (owner, boss_scope, state);
-- The scope an "always" answer to an ask makes its preference for.
ALTER TABLE boss_open_items ADD COLUMN prefer_scope TEXT;
