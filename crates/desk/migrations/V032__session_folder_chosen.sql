-- A session the person started in Desk in a folder they chose: their
-- consent for its CLI to trust that folder, decided when it was made and
-- kept for its resumes. Sessions from before were never asked: none has it.
ALTER TABLE sessions ADD COLUMN folder_chosen BOOLEAN NOT NULL DEFAULT false;
