-- This computer's readings are kept by the account they are of: one CLI
-- sign-in and each of the person's Desk accounts were one row ('mac:claude'),
-- so one account's reading was filed as another's. Those rows are dropped;
-- each account's reading is read again with its next report.
DELETE FROM floor.plan_readings WHERE login IN ('mac:claude', 'mac:chatgpt');
