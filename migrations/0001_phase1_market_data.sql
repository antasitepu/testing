CREATE TABLE markets (
    symbol TEXT PRIMARY KEY,
    enabled BOOLEAN NOT NULL DEFAULT TRUE,
    status TEXT NOT NULL CHECK (status IN ('TRADING', 'HALTED', 'UNKNOWN')),
    price_tick NUMERIC(38, 18) NOT NULL CHECK (price_tick > 0),
    onboard_time BIGINT NOT NULL CHECK (onboard_time >= 0),
    session_market TEXT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE candles (
    symbol TEXT NOT NULL REFERENCES markets(symbol) ON DELETE CASCADE,
    interval TEXT NOT NULL CHECK (interval IN ('1m', '5m', '15m', '30m', '1h', '4h', '1d')),
    open_time BIGINT NOT NULL,
    close_time BIGINT NOT NULL CHECK (close_time >= open_time),
    open NUMERIC(38, 18) NOT NULL CHECK (open > 0),
    high NUMERIC(38, 18) NOT NULL CHECK (high > 0),
    low NUMERIC(38, 18) NOT NULL CHECK (low > 0),
    close NUMERIC(38, 18) NOT NULL CHECK (close > 0),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (symbol, interval, open_time),
    CHECK (high >= open AND high >= close),
    CHECK (low <= open AND low <= close)
);

CREATE INDEX candles_history_idx
    ON candles (symbol, interval, open_time DESC);

CREATE TABLE sync_states (
    symbol TEXT NOT NULL REFERENCES markets(symbol) ON DELETE CASCADE,
    interval TEXT NOT NULL CHECK (interval IN ('1m', '5m', '15m', '30m', '1h', '4h', '1d')),
    oldest_synced_time BIGINT,
    latest_synced_time BIGINT,
    full_history_complete BOOLEAN NOT NULL DEFAULT FALSE,
    recent_sync_status TEXT NOT NULL DEFAULT 'pending'
        CHECK (recent_sync_status IN ('pending', 'running', 'complete', 'failed')),
    backfill_status TEXT NOT NULL DEFAULT 'pending'
        CHECK (backfill_status IN ('pending', 'running', 'complete', 'failed')),
    last_error TEXT,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (symbol, interval)
);

CREATE TABLE market_sessions (
    session_market TEXT NOT NULL,
    session_status TEXT NOT NULL CHECK (session_status IN ('REGULAR', 'NO_TRADING')),
    session_start_time BIGINT NOT NULL,
    session_end_time BIGINT NOT NULL CHECK (session_end_time > session_start_time),
    source_event_time BIGINT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (
        session_market,
        session_start_time,
        session_end_time,
        session_status
    )
);

CREATE INDEX market_sessions_lookup_idx
    ON market_sessions (session_market, session_start_time, session_end_time);
