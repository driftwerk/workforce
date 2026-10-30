-- An allow is one tool's writes to a destination, not every write there:
-- an allow kept before it named its tool is dropped, and asked again.
DELETE FROM surface.allows;
ALTER TABLE surface.allows ADD COLUMN tool TEXT NOT NULL;
ALTER TABLE surface.allows
    DROP CONSTRAINT allows_person_agent_id_platform_destination_key,
    ADD CONSTRAINT allows_person_agent_id_tool_platform_destination_key
        UNIQUE (person, agent_id, tool, platform, destination);
