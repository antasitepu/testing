WITH latest_target AS (
    SELECT DISTINCT ON (virtual_trade_id)
           virtual_trade_id,
           (payload->>'primaryTarget')::NUMERIC AS primary_target
    FROM virtual_trade_events
    WHERE event_type = 'TARGET_MOVED'
      AND payload ? 'primaryTarget'
    ORDER BY virtual_trade_id, sequence DESC
)
UPDATE virtual_trades AS trade
SET primary_target = latest.primary_target
FROM latest_target AS latest
WHERE trade.virtual_trade_id = latest.virtual_trade_id
  AND trade.primary_target IS DISTINCT FROM latest.primary_target;
