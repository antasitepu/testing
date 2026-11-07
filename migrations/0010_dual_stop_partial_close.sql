ALTER TABLE analysis_profiles
ADD COLUMN position_management JSONB NOT NULL DEFAULT '{}'::jsonb;

ALTER TABLE virtual_trades
ADD COLUMN position_stage TEXT NOT NULL DEFAULT 'LEGACY',
ADD COLUMN trade_health TEXT NOT NULL DEFAULT 'HEALTHY',
ADD COLUMN thesis_invalidation NUMERIC(38,18),
ADD COLUMN tp1_price NUMERIC(38,18),
ADD COLUMN tp1_r NUMERIC(38,18),
ADD COLUMN tp1_fraction NUMERIC(38,18),
ADD COLUMN tp1_filled_at BIGINT,
ADD COLUMN remaining_fraction NUMERIC(38,18) NOT NULL DEFAULT 1,
ADD COLUMN realized_fraction NUMERIC(38,18) NOT NULL DEFAULT 0,
ADD COLUMN realized_r NUMERIC(38,18) NOT NULL DEFAULT 0,
ADD COLUMN tp2_zone_low NUMERIC(38,18),
ADD COLUMN tp2_zone_high NUMERIC(38,18),
ADD CONSTRAINT virtual_trades_position_stage_check
    CHECK (position_stage IN ('LEGACY', 'FULL_POSITION', 'RUNNER')),
ADD CONSTRAINT virtual_trades_trade_health_check
    CHECK (trade_health IN ('HEALTHY', 'WEAKENING', 'INVALID')),
ADD CONSTRAINT virtual_trades_tp1_fraction_check
    CHECK (tp1_fraction IS NULL OR tp1_fraction BETWEEN 0 AND 1),
ADD CONSTRAINT virtual_trades_remaining_fraction_check
    CHECK (remaining_fraction BETWEEN 0 AND 1),
ADD CONSTRAINT virtual_trades_realized_fraction_check
    CHECK (realized_fraction BETWEEN 0 AND 1),
ADD CONSTRAINT virtual_trades_fraction_total_check
    CHECK (remaining_fraction + realized_fraction = 1),
ADD CONSTRAINT virtual_trades_tp2_zone_check
    CHECK (
        (tp2_zone_low IS NULL AND tp2_zone_high IS NULL)
        OR (
            tp2_zone_low IS NOT NULL
            AND tp2_zone_high IS NOT NULL
            AND tp2_zone_low <= tp2_zone_high
        )
    );
