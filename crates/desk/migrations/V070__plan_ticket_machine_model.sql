-- Where a plan's ticket runs, beside its engine, folder and view: the
-- person's machine (`devbox`; NULL is this computer, whose folder it is)
-- and the model its session runs (NULL is its engine's default).
-- Additive: two nullable columns.
ALTER TABLE tickets ADD COLUMN machine TEXT;
ALTER TABLE tickets ADD COLUMN model TEXT;
