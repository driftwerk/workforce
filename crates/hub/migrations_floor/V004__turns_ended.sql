-- A turn that ended within a day is read however long ago it began.
CREATE INDEX turns_owner_ended ON floor.turns (owner, ended_at DESC) WHERE ended_at IS NOT NULL;
