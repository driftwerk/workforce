-- Definition sharing (workflows, playbooks, monitors).
--
-- A definition is owned by one identity (`workflow_defs.owner_identity_id`) and
-- is `private` (owner only) or `global` (anyone may run/view). This adds the
-- missing middle tier: an owner shares a specific definition with specific users
-- or teams, at one of two levels — `run` (read + run) or `manage` (run + edit +
-- delete). Sharing is an ACL on the owner's row, never a copy, so there is one
-- source of truth.
--
-- One table covers all kinds (it carries `kind`), mirroring the single
-- `workflow_defs`/`def_revisions` spine. `principal_type` is `user` (grantee =
-- identity UUID) or `team` (grantee = team UUID, membership resolved at check
-- time). The FK to `workflow_defs` cascades, so deleting a definition drops its
-- grants. System-owned definitions (no owner) are never shared; sharing is
-- gated on a real owner.
CREATE TABLE IF NOT EXISTS def_shares (
    kind              TEXT NOT NULL,
    def_id            TEXT NOT NULL,
    owner_identity_id TEXT NOT NULL,
    principal_type    TEXT NOT NULL,
    principal_id      TEXT NOT NULL,
    permission        TEXT NOT NULL,
    granted_by        TEXT NOT NULL,
    granted_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (kind, def_id, owner_identity_id, principal_type, principal_id),
    FOREIGN KEY (def_id, kind, owner_identity_id)
        REFERENCES workflow_defs (id, kind, owner_identity_id) ON DELETE CASCADE
);

-- Reverse lookup: every definition shared to a given principal (the caller's
-- identity and each of their teams), used to widen list/visibility queries.
CREATE INDEX IF NOT EXISTS idx_def_shares_principal
    ON def_shares (principal_type, principal_id, kind);
