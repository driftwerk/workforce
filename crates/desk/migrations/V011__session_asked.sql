-- Why a session's turn is waiting on the person, while it is: what it
-- asked, why Desk read it so, and the refused tool it stopped on. Kept so a
-- session the PTY holder carried through a hub restart still says.
ALTER TABLE sessions ADD COLUMN asked JSONB;
