ALTER TABLE virtual_trades
DROP CONSTRAINT virtual_trades_tp1_fraction_check,
ADD CONSTRAINT virtual_trades_tp1_fraction_check
    CHECK (tp1_fraction IS NULL OR tp1_fraction = 0.5) NOT VALID;
