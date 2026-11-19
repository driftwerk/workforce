-- The repository a plan's ticket works in, by where it comes from
-- (`github.com/driftwerk/vellev`): its folder on whichever computer it runs
-- on is that repository's checkout there.
ALTER TABLE tickets ADD COLUMN repo TEXT;
