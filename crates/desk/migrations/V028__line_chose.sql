-- What a session said it goes ahead with when its question isn't answered
-- in time (an `ask`'s `default`), shown beside the question.
ALTER TABLE line_messages ADD COLUMN chose TEXT;
