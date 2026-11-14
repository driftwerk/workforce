-- What each of the person's machines last said it spent today for a boss on
-- this hub that runs its sessions there at Act fully: one daily budget is
-- counted across this computer and every machine. `count_from` is where the
-- day's count starts here (midnight, or when the person cleared it); a row
-- whose `count_from` isn't the current one counts nothing. Kept so a restart
-- of the hub doesn't forget what a machine that is offline now spent.
CREATE TABLE machine_spent (
    agent      TEXT             NOT NULL,
    machine    TEXT             NOT NULL,
    count_from TIMESTAMPTZ      NOT NULL,
    usd        DOUBLE PRECISION NOT NULL,
    updated_at TIMESTAMPTZ      NOT NULL,
    PRIMARY KEY (agent, machine)
);
