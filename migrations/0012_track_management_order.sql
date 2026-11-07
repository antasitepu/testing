ALTER TABLE virtual_trades
ADD COLUMN last_management_effective_at BIGINT,
ADD COLUMN last_management_candle_time BIGINT;
