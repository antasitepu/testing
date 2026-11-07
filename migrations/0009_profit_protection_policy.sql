ALTER TABLE analysis_profiles
ADD COLUMN profit_protection JSONB NOT NULL DEFAULT '{}'::jsonb;
