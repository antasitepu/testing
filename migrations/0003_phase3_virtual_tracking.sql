CREATE TABLE market_event_cursors (
    symbol TEXT PRIMARY KEY REFERENCES markets(symbol),
    last_processed_agg_trade_id BIGINT NOT NULL,
    last_trade_time BIGINT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE virtual_trades (
    virtual_trade_id UUID PRIMARY KEY,
    signal_id UUID NOT NULL UNIQUE REFERENCES signals(signal_id),
    symbol TEXT NOT NULL,
    plan TEXT NOT NULL,
    interval TEXT NOT NULL,
    direction TEXT NOT NULL CHECK (direction IN ('BUY', 'SELL')),
    trade_state TEXT NOT NULL CHECK (trade_state IN ('OPEN', 'CLOSED')),
    actual_entry_time BIGINT NOT NULL,
    actual_entry_price NUMERIC(38,18) NOT NULL,
    exit_time BIGINT,
    exit_price NUMERIC(38,18),
    primary_target NUMERIC(38,18) NOT NULL,
    reference_levels JSONB NOT NULL,
    initial_stop NUMERIC(38,18) NOT NULL,
    active_stop NUMERIC(38,18) NOT NULL,
    release_id UUID NOT NULL,
    algorithm_version TEXT NOT NULL,
    parameter_version TEXT NOT NULL,
    execution_model TEXT NOT NULL,
    reconciliation_source TEXT,
    ambiguity TEXT NOT NULL DEFAULT 'NONE',
    result JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    closed_at TIMESTAMPTZ
);

CREATE INDEX virtual_trades_history_idx
    ON virtual_trades(symbol, plan, interval, closed_at DESC);

CREATE TABLE virtual_trade_events (
    event_id UUID PRIMARY KEY,
    virtual_trade_id UUID NOT NULL REFERENCES virtual_trades(virtual_trade_id),
    sequence BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    event_time BIGINT NOT NULL,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (virtual_trade_id, sequence)
);

CREATE TABLE tracker_states (
    signal_id UUID PRIMARY KEY REFERENCES signals(signal_id),
    symbol TEXT NOT NULL,
    state JSONB NOT NULL,
    tracking_health TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX monitoring_retention_idx
    ON monitoring_events(created_at)
    WHERE published_at IS NOT NULL;
