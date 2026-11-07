use analysis_domain::PriceZone;
use execution_domain::TrackingHealth;
use market_domain::AggregateTrade;

pub fn proves_zone_touch(zone: &PriceZone, event: &AggregateTrade) -> bool {
    event.price >= zone.low && event.price <= zone.high
}

pub fn proves_effective_zone_touch(
    zone: &PriceZone,
    event: &AggregateTrade,
    effective_at: i64,
    invalidated: bool,
) -> bool {
    !invalidated && event.trade_time >= effective_at && proves_zone_touch(zone, event)
}

#[derive(Clone, Debug, Default)]
pub struct LimitZoneGuard {
    last_aggregate_trade_id: Option<i64>,
}

impl LimitZoneGuard {
    pub fn observe(
        &mut self,
        zone: &PriceZone,
        event: &AggregateTrade,
        effective_at: i64,
        invalidated: bool,
    ) -> Result<LimitZoneDecision, LimitZoneError> {
        if let Some(previous_id) = self.last_aggregate_trade_id {
            if event.aggregate_trade_id <= previous_id {
                return Ok(LimitZoneDecision::Duplicate);
            }
            if event.aggregate_trade_id != previous_id + 1 {
                return Err(LimitZoneError::SequenceGap {
                    previous_id,
                    next_id: event.aggregate_trade_id,
                });
            }
        }
        self.last_aggregate_trade_id = Some(event.aggregate_trade_id);
        if proves_effective_zone_touch(zone, event, effective_at, invalidated) {
            return Ok(LimitZoneDecision::Enter {
                aggregate_trade_id: event.aggregate_trade_id,
                time: event.trade_time,
                price: event.price,
            });
        }
        Ok(LimitZoneDecision::Wait)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LimitZoneDecision {
    Enter {
        aggregate_trade_id: i64,
        time: i64,
        price: rust_decimal::Decimal,
    },
    Duplicate,
    Wait,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum LimitZoneError {
    #[error("aggregate trade sequence gap from {previous_id} to {next_id}")]
    SequenceGap { previous_id: i64, next_id: i64 },
}

impl LimitZoneError {
    pub const fn tracking_health(&self) -> TrackingHealth {
        TrackingHealth::Stale
    }
}
