-- Which paired device answered an ask, for each decided call a person
-- answered: so a card answered from somewhere other than Desk on this
-- computer shows in the activity.
ALTER TABLE access_activity ADD COLUMN device TEXT;
