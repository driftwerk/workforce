-- What each agent does on its own on this computer, as the person set it: a
-- level for a kind of action (`scope` empty), or one folder, host or site it
-- does that kind in on its own. No row is "ask each time". Only Desk's own
-- token writes it, and it applies only to the person's own turns.
CREATE TABLE access_on_its_own (
    hub        TEXT        NOT NULL DEFAULT 'local',
    agent      TEXT        NOT NULL,
    kind       TEXT        NOT NULL,   -- commands | network | browser | files | delete
    scope      TEXT        NOT NULL DEFAULT '',   -- '' | folder:<path> | host:<host> | site:<origin>
    level      TEXT        NOT NULL,   -- in_folders | anywhere
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (hub, agent, kind, scope)
);
