-- A run changed outside its execute loop (a decision, a resume, an operator
-- write) is saved only at the version it was read at, so two such changes can
-- never both apply. Every content write moves the version on; the lease does
-- not.
ALTER TABLE workflow_instances ADD COLUMN IF NOT EXISTS version BIGINT NOT NULL DEFAULT 0;
