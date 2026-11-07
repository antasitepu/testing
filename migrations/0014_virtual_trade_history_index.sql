CREATE INDEX virtual_trades_symbol_entry_history_idx
    ON virtual_trades(symbol, actual_entry_time DESC, virtual_trade_id DESC);
