-- Session questions given to the person while they were at that session a
-- moment before (overseer, the brief update): said once they have left it
-- for its boss's answer gap, unless they answer there first. Kept here so a
-- restart still says them. Additive: a new table.
CREATE TABLE boss_held_questions (
    line_message UUID        PRIMARY KEY,
    held_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
