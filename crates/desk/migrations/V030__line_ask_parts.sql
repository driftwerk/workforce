-- An ask said in parts: its work item (`about`), a one-line reason (`why`)
-- and its details, beside its one-line question in `text`. Older rows keep
-- them empty; Desk splits their text when it shows them.
ALTER TABLE line_messages ADD COLUMN about TEXT;
ALTER TABLE line_messages ADD COLUMN why TEXT;
ALTER TABLE line_messages ADD COLUMN details TEXT;
