-- Who an in-flight turn runs as and bills to, beside its turn record, so a
-- turn resumed after a restart keeps the principal it started with (a
-- scheduled job's run-as identity, an explicit billing lane) instead of
-- falling back to the session's owner. The principal belongs to the turn
-- whose token it was written with: a replica that predates these columns
-- replaces the token without them, and its turn resumes as the owner, as do
-- records from before.
ALTER TABLE sessions
    ADD COLUMN IF NOT EXISTS turn_run_as          TEXT,
    ADD COLUMN IF NOT EXISTS turn_bill_to         TEXT,
    ADD COLUMN IF NOT EXISTS turn_principal_token TEXT;
