CREATE TABLE plan_feedback (
    feedback_id UUID PRIMARY KEY,
    signal_id UUID NOT NULL REFERENCES signals(signal_id),
    virtual_trade_id UUID NOT NULL,
    feedback_type TEXT NOT NULL CHECK (feedback_type IN ('ACTUAL_ENTRY', 'TRADE_CLOSED')),
    payload JSONB NOT NULL,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW(),
    consumed_at TIMESTAMPTZ,
    UNIQUE (signal_id, virtual_trade_id, feedback_type)
);

CREATE INDEX plan_feedback_pending_idx
    ON plan_feedback(signal_id, created_at)
    WHERE consumed_at IS NULL;
