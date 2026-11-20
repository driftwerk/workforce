-- Why a plan's ticket waits in To do for someone to start it: its session
-- ended as it started, so its plan doesn't start it again by itself. Gone
-- once it is started, handed out, moved to another column or leaves its
-- plan. Additive: null for every ticket there is.
ALTER TABLE tickets ADD COLUMN held_back TEXT;
