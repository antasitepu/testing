CREATE INDEX tracker_states_entry_available_idx
    ON tracker_states(signal_id)
    WHERE state->>'setupState' = 'ENTRY_AVAILABLE'
      AND state->>'tradeState' = 'NOT_ENTERED';

CREATE INDEX virtual_trades_open_signal_idx
    ON virtual_trades(signal_id)
    WHERE trade_state = 'OPEN';

CREATE INDEX trade_directives_pending_signal_effective_idx
    ON trade_management_directives(signal_id, effective_at, directive_id)
    WHERE consumed_at IS NULL;
