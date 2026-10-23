-- Include `kind` in the definition key.
--
-- Definitions (workflows, playbooks, monitors, identity files, consolidation
-- templates) share one `workflow_defs` table keyed by (id, owner_identity_id).
-- Without `kind` in the key, a member who reuses the same id across kinds
-- (e.g. a playbook and a consolidation_template both named "group") overwrites
-- the first, because the save/seed upsert conflicts on (id, owner_identity_id)
-- only. Widen the key to (id, kind, owner_identity_id) so different-kind defs
-- with the same id and owner coexist; reads/deletes/revisions already filter by
-- kind, so they simply stop aliasing across kinds.
--
-- Existing rows are already unique on the narrower (id, owner_identity_id), so
-- they are trivially unique under the wider key — no data fix-up, and the
-- ADD PRIMARY KEY cannot hit a duplicate. def_revisions is keyed independently
-- on (kind, ref_id, owner_identity_id, rev) and is unaffected.

ALTER TABLE workflow_defs DROP CONSTRAINT IF EXISTS workflow_defs_pkey;
ALTER TABLE workflow_defs ADD PRIMARY KEY (id, kind, owner_identity_id);
