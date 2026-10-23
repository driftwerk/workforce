-- Which code graph this is: an id made once, when the graph is. What a
-- graph keeps elsewhere (its code vectors, in a vector database) is kept
-- as this graph's, so that two graphs holding repos of one id do not take
-- each other's for their own.
CREATE TABLE IF NOT EXISTS graph_identity (
    only_one BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (only_one),
    id       TEXT NOT NULL
);

INSERT INTO graph_identity (id)
SELECT md5(random()::text || clock_timestamp()::text)
ON CONFLICT (only_one) DO NOTHING;
