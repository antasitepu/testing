CREATE TABLE scalping_audit_snapshots (
    snapshot_id UUID PRIMARY KEY,
    symbol TEXT NOT NULL REFERENCES markets(symbol) ON DELETE CASCADE,
    mode TEXT NOT NULL,
    timeframes TEXT[] NOT NULL,
    report JSONB NOT NULL,
    journal JSONB NOT NULL,
    audit JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);

CREATE INDEX scalping_audit_snapshots_symbol_created_idx
    ON scalping_audit_snapshots(symbol, created_at DESC);
