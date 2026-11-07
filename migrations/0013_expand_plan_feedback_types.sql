ALTER TABLE plan_feedback
DROP CONSTRAINT plan_feedback_feedback_type_check,
ADD CONSTRAINT plan_feedback_feedback_type_check
    CHECK (feedback_type IN ('ACTUAL_ENTRY', 'PARTIAL_TARGET_FILLED', 'TRADE_CLOSED'));
