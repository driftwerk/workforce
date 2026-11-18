-- The cheap model's calls a subscription plan served (a Claude or ChatGPT
-- login), counted apart from `calls`: those cost nothing metered, so no
-- ceiling holds them, and they are counted only to be shown. `calls` is
-- now the metered ones alone, which `cheap_calls_per_day` and
-- `account_questions_per_day` still hold.
ALTER TABLE desk_cheap_calls ADD COLUMN plan_calls INT NOT NULL DEFAULT 0;
