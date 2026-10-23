-- Approval routing & designated approvers (additive).
--
-- Mirrors `crates/permissions/src/sqlite.rs::add_missing_columns`. The
-- two columns carry routed-approval config on each rule; defaults keep
-- every existing row decoding unchanged.

ALTER TABLE permission_rules
    ADD COLUMN IF NOT EXISTS approvers TEXT NOT NULL DEFAULT '[]';

ALTER TABLE permission_rules
    ADD COLUMN IF NOT EXISTS allow_self_approval BOOLEAN NOT NULL DEFAULT true;
