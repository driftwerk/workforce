-- A synced ticket's place on its Jira or Linear board, as a string that
-- sorts (byte-wise) the way the source sorts: Jira's Rank (a LexoRank) as
-- it is, Linear's sortOrder as sortable hex. Null on a ticket of Desk's own,
-- or one whose issue's place hasn't been read yet.
ALTER TABLE tickets ADD COLUMN upstream_rank TEXT;
