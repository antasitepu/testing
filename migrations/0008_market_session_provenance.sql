ALTER TABLE market_sessions
    ADD COLUMN source_kind TEXT NOT NULL DEFAULT 'BINANCE_API',
    ADD COLUMN source_reference TEXT,
    ADD CONSTRAINT market_sessions_source_kind_check
        CHECK (source_kind IN ('BINANCE_API', 'DERIVED_WEEKLY', 'MANUAL_OVERRIDE'));

CREATE INDEX market_sessions_source_idx
    ON market_sessions (session_market, source_kind, session_start_time);
