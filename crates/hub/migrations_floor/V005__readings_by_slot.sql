-- Hub readings are kept by credential and slot: the earlier keys were the
-- credential's alone, and a reading is read again on the next call.
DELETE FROM floor.plan_readings WHERE login LIKE 'hub:%';
