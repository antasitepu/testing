CREATE TABLE configuration_releases (
    release_id UUID PRIMARY KEY,
    symbol TEXT NOT NULL REFERENCES markets(symbol),
    mode TEXT NOT NULL CHECK (mode IN ('RESEARCH', 'PAPER_ACTIVE')),
    lifecycle TEXT NOT NULL CHECK (lifecycle IN ('DRAFT', 'ACTIVE', 'RETIRED')),
    validation_summary JSONB NOT NULL DEFAULT '{}'::jsonb,
    activated_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE UNIQUE INDEX one_active_analysis_release
    ON configuration_releases(symbol)
    WHERE lifecycle = 'ACTIVE';

CREATE TABLE analysis_profiles (
    release_id UUID NOT NULL REFERENCES configuration_releases(release_id),
    symbol TEXT NOT NULL REFERENCES markets(symbol),
    plan TEXT NOT NULL,
    interval TEXT NOT NULL,
    enabled BOOLEAN NOT NULL,
    parameter_version TEXT NOT NULL,
    algorithm_version TEXT NOT NULL,
    parameters JSONB NOT NULL,
    required_timeframes JSONB NOT NULL,
    minimum_history_bars INTEGER NOT NULL CHECK (minimum_history_bars > 0),
    warmup_bars INTEGER NOT NULL CHECK (warmup_bars >= 0),
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (release_id, symbol, plan, interval)
);

CREATE TABLE analysis_runtime_state (
    symbol TEXT PRIMARY KEY REFERENCES markets(symbol),
    live_analysis_start_at BIGINT NOT NULL,
    active_release_id UUID REFERENCES configuration_releases(release_id),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE analysis_work_items (
    work_id UUID PRIMARY KEY,
    symbol TEXT NOT NULL REFERENCES markets(symbol),
    interval TEXT NOT NULL,
    close_time BIGINT NOT NULL,
    release_id UUID NOT NULL REFERENCES configuration_releases(release_id),
    source TEXT NOT NULL CHECK (source IN ('LIVE', 'CATCH_UP', 'DOWNTIME_RECONCILED')),
    status TEXT NOT NULL CHECK (status IN ('PENDING', 'LEASED', 'COMPLETED', 'FAILED')),
    lease_owner TEXT,
    lease_expires_at TIMESTAMPTZ,
    attempt_count INTEGER NOT NULL DEFAULT 0 CHECK (attempt_count >= 0),
    last_error JSONB,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    completed_at TIMESTAMPTZ,
    UNIQUE (symbol, interval, close_time, release_id)
);

CREATE INDEX analysis_work_pending_idx
    ON analysis_work_items(status, close_time, created_at)
    WHERE status IN ('PENDING', 'LEASED');

CREATE TABLE analysis_evaluations (
    symbol TEXT NOT NULL,
    plan TEXT NOT NULL,
    interval TEXT NOT NULL,
    release_id UUID NOT NULL,
    algorithm_version TEXT NOT NULL,
    parameter_version TEXT NOT NULL,
    evaluated_closed_candle_time BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (
        symbol,
        plan,
        interval,
        release_id,
        algorithm_version,
        parameter_version,
        evaluated_closed_candle_time
    )
);

CREATE TABLE plan_runtime_states (
    symbol TEXT NOT NULL,
    plan TEXT NOT NULL,
    interval TEXT NOT NULL,
    release_id UUID NOT NULL,
    state JSONB NOT NULL,
    last_evaluated_close_time BIGINT NOT NULL,
    updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    PRIMARY KEY (symbol, plan, interval)
);

CREATE TABLE signals (
    signal_id UUID PRIMARY KEY,
    symbol TEXT NOT NULL,
    plan TEXT NOT NULL,
    interval TEXT NOT NULL,
    definition JSONB NOT NULL,
    evidence JSONB NOT NULL,
    release_id UUID NOT NULL,
    algorithm_version TEXT NOT NULL,
    parameter_version TEXT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX signals_history_idx
    ON signals(symbol, plan, interval, created_at DESC, signal_id DESC);

CREATE TABLE signal_events (
    event_id UUID PRIMARY KEY,
    signal_id UUID NOT NULL REFERENCES signals(signal_id),
    sequence BIGINT NOT NULL,
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL,
    effective_at BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    UNIQUE (signal_id, sequence)
);

CREATE TABLE trade_management_directives (
    directive_id UUID PRIMARY KEY,
    signal_id UUID NOT NULL REFERENCES signals(signal_id),
    virtual_trade_id UUID,
    kind TEXT NOT NULL,
    payload JSONB NOT NULL,
    effective_at BIGINT NOT NULL,
    source_closed_candle_time BIGINT NOT NULL,
    algorithm_version TEXT NOT NULL,
    parameter_version TEXT NOT NULL,
    consumed_at TIMESTAMPTZ
);

CREATE INDEX trade_directives_pending_idx
    ON trade_management_directives(effective_at, directive_id)
    WHERE consumed_at IS NULL;

CREATE TABLE rejected_plan_candidates (
    candidate_id UUID PRIMARY KEY,
    symbol TEXT NOT NULL,
    plan TEXT NOT NULL,
    interval TEXT NOT NULL,
    reason TEXT NOT NULL,
    payload JSONB NOT NULL,
    evaluated_at BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE SEQUENCE monitoring_event_cursor_seq;

CREATE TABLE monitoring_events (
    cursor BIGINT PRIMARY KEY DEFAULT nextval('monitoring_event_cursor_seq'),
    event_id UUID NOT NULL UNIQUE,
    event_type TEXT NOT NULL,
    entity_type TEXT NOT NULL,
    entity_id UUID,
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    published_at TIMESTAMPTZ,
    publish_attempts INTEGER NOT NULL DEFAULT 0,
    last_publish_error TEXT
);

CREATE INDEX monitoring_unpublished_idx
    ON monitoring_events(cursor)
    WHERE published_at IS NULL;

CREATE TABLE backtest_runs (
    run_id UUID PRIMARY KEY,
    status TEXT NOT NULL CHECK (status IN ('PENDING', 'RUNNING', 'COMPLETED', 'FAILED')),
    input JSONB NOT NULL,
    metrics JSONB,
    error JSONB,
    code_commit_hash TEXT NOT NULL,
    started_at TIMESTAMPTZ,
    completed_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE TABLE backtest_trades (
    trade_id UUID PRIMARY KEY,
    run_id UUID NOT NULL REFERENCES backtest_runs(run_id),
    sequence BIGINT NOT NULL,
    trade JSONB NOT NULL,
    UNIQUE (run_id, sequence)
);

CREATE INDEX backtest_trades_run_idx
    ON backtest_trades(run_id, sequence);

CREATE FUNCTION reject_non_draft_profile_change()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
DECLARE
    release_lifecycle TEXT;
BEGIN
    SELECT lifecycle INTO release_lifecycle
    FROM configuration_releases
    WHERE release_id = OLD.release_id;

    IF release_lifecycle <> 'DRAFT' THEN
        RAISE EXCEPTION 'active or retired analysis profile rows are immutable';
    END IF;

    RETURN COALESCE(NEW, OLD);
END;
$$;

CREATE TRIGGER activated_analysis_profiles_are_immutable
BEFORE UPDATE OR DELETE ON analysis_profiles
FOR EACH ROW EXECUTE FUNCTION reject_non_draft_profile_change();

CREATE FUNCTION reject_immutable_analysis_row_change()
RETURNS TRIGGER
LANGUAGE plpgsql
AS $$
BEGIN
    RAISE EXCEPTION '% rows are immutable', TG_TABLE_NAME;
END;
$$;

CREATE TRIGGER signals_are_immutable
BEFORE UPDATE OR DELETE ON signals
FOR EACH ROW EXECUTE FUNCTION reject_immutable_analysis_row_change();
