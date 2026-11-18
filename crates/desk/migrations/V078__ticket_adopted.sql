-- A plan's ticket taken on from a board rather than made by the plan: one
-- of the person's own, or a synced Linear or Jira issue. The plan never
-- deletes, drops, renames or re-files it; dropping the plan, or the ticket
-- from the plan, lets it go where it is. Additive: one column, false for
-- every ticket there is.
ALTER TABLE tickets ADD COLUMN adopted BOOLEAN NOT NULL DEFAULT false;
-- The plan that added a need, when one did: letting a ticket go removes
-- only those, never a need the person or the tracker had. Null for every
-- need there is.
ALTER TABLE ticket_needs ADD COLUMN plan UUID;
