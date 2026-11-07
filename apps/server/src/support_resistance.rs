use analysis_core::{LiquidityPool, LiquiditySide};
use analysis_domain::{ConfirmedFact, FactKind, FactLifecycle};
use market_domain::{Candle, Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupportResistanceZoneDto {
    pub zone_id: String,
    pub symbol: Symbol,
    pub timeframe: Timeframe,
    pub kind: SupportResistanceKind,
    pub low: String,
    pub high: String,
    pub level: String,
    pub confirmed_at: i64,
    pub source: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SupportResistanceKind {
    Support,
    Resistance,
}

pub fn active_zones_from_facts(
    facts: &[ConfirmedFact<Value>],
    candles: &[Candle],
    limit: usize,
) -> Vec<SupportResistanceZoneDto> {
    let mut candidates = facts
        .iter()
        .filter(|fact| fact.id.kind == FactKind::Liquidity)
        .filter(|fact| fact.lifecycle == FactLifecycle::Confirmed)
        .filter_map(|fact| {
            let pool = serde_json::from_value::<LiquidityPool>(fact.value.clone()).ok()?;
            if !is_valid(&pool, candles) {
                return None;
            }
            let kind = match pool.side {
                LiquiditySide::SellSide => SupportResistanceKind::Support,
                LiquiditySide::BuySide => SupportResistanceKind::Resistance,
            };
            Some(SupportResistanceCandidate {
                low: pool.lower_bound,
                high: pool.upper_bound,
                zone: SupportResistanceZoneDto {
                    zone_id: fact_id_key(&pool),
                    symbol: pool.id.symbol.clone(),
                    timeframe: pool.id.timeframe,
                    kind,
                    low: pool.lower_bound.to_string(),
                    high: pool.upper_bound.to_string(),
                    level: pool.level.to_string(),
                    confirmed_at: pool.id.confirmed_at,
                    source: "LIQUIDITY_POOL",
                },
            })
        })
        .collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| std::cmp::Reverse(candidate.zone.confirmed_at));
    let mut ranked = Vec::new();
    for candidate in candidates {
        if ranked
            .iter()
            .any(|existing| duplicate_zone(existing, &candidate))
        {
            continue;
        }
        ranked.push(candidate);
        if ranked.len() >= limit {
            break;
        }
    }
    ranked.into_iter().map(|candidate| candidate.zone).collect()
}

pub fn latest_close(candles: &[Candle]) -> Option<Decimal> {
    candles
        .iter()
        .max_by_key(|candle| candle.close_time)
        .map(|candle| candle.close)
}

fn is_valid(pool: &LiquidityPool, candles: &[Candle]) -> bool {
    candles
        .iter()
        .filter(|candle| candle.close_time > pool.id.confirmed_at)
        .all(|candle| match pool.side {
            LiquiditySide::SellSide => candle.low >= pool.lower_bound,
            LiquiditySide::BuySide => candle.high <= pool.upper_bound,
        })
}

struct SupportResistanceCandidate {
    zone: SupportResistanceZoneDto,
    low: Decimal,
    high: Decimal,
}

fn duplicate_zone(
    existing: &SupportResistanceCandidate,
    candidate: &SupportResistanceCandidate,
) -> bool {
    let gap = dedupe_price_gap();
    existing.zone.kind == candidate.zone.kind
        && candidate.low <= existing.high + gap
        && existing.low <= candidate.high + gap
}

fn dedupe_price_gap() -> Decimal {
    Decimal::new(20, 2)
}

fn fact_id_key(pool: &LiquidityPool) -> String {
    format!(
        "{}:{}:{}:{}",
        pool.id.timeframe.as_str(),
        pool.id.confirmed_at,
        pool.id.ordinal,
        match pool.side {
            LiquiditySide::SellSide => "support",
            LiquiditySide::BuySide => "resistance",
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use analysis_core::LiquidityPool;
    use analysis_domain::{FactId, FactKind};
    use market_domain::Symbol;

    fn fact(side: LiquiditySide, low: i64, high: i64, ordinal: u32) -> ConfirmedFact<Value> {
        let pool = LiquidityPool {
            id: FactId {
                symbol: Symbol::new("XAUUSDT").unwrap(),
                timeframe: Timeframe::M1,
                kind: FactKind::Liquidity,
                confirmed_at: i64::from(ordinal) * 60_000,
                ordinal,
                algorithm_version: "liquidity-v1".to_owned(),
                parameter_version: "liquidity-1m-v1".to_owned(),
            },
            side,
            level: Decimal::new(low + high, 1) / Decimal::TWO,
            lower_bound: Decimal::new(low, 0),
            upper_bound: Decimal::new(high, 0),
            source_swing_ids: Vec::new(),
            lifecycle: FactLifecycle::Confirmed,
        };
        ConfirmedFact {
            id: pool.id.clone(),
            value: serde_json::to_value(pool).unwrap(),
            lifecycle: FactLifecycle::Confirmed,
        }
    }

    fn candle(open_time: i64, low: i64, high: i64, close: i64) -> Candle {
        Candle::new(
            Symbol::new("XAUUSDT").unwrap(),
            Timeframe::M1,
            open_time,
            open_time + 59_999,
            Decimal::new(close, 0),
            Decimal::new(high, 0),
            Decimal::new(low, 0),
            Decimal::new(close, 0),
        )
        .unwrap()
    }

    #[test]
    fn only_valid_support_and_resistance_zones_are_returned() {
        let zones = active_zones_from_facts(
            &[
                fact(LiquiditySide::SellSide, 100, 102, 1),
                fact(LiquiditySide::SellSide, 110, 112, 2),
                fact(LiquiditySide::BuySide, 120, 122, 3),
                fact(LiquiditySide::BuySide, 90, 92, 4),
            ],
            &[candle(300_000, 105, 105, 105)],
            10,
        );

        assert_eq!(
            zones.iter().map(|zone| zone.kind).collect::<Vec<_>>(),
            vec![
                SupportResistanceKind::Resistance,
                SupportResistanceKind::Support
            ]
        );
        assert_eq!(zones[0].low, "120");
        assert_eq!(zones[1].high, "102");
    }

    #[test]
    fn invalidates_zone_once_future_wick_breaks_the_outer_bound() {
        let zones = active_zones_from_facts(
            &[
                fact(LiquiditySide::SellSide, 100, 102, 1),
                fact(LiquiditySide::BuySide, 120, 122, 2),
            ],
            &[
                candle(180_000, 99, 106, 105),
                candle(240_000, 104, 123, 105),
            ],
            10,
        );

        assert!(zones.is_empty());
    }

    #[test]
    fn deduplicates_nearby_zones_after_ranking_newest_first() {
        let zones = active_zones_from_facts(
            &[
                fact(LiquiditySide::SellSide, 100, 102, 1),
                fact(LiquiditySide::SellSide, 101, 103, 2),
                fact(LiquiditySide::SellSide, 110, 112, 3),
            ],
            &[candle(300_000, 120, 120, 120)],
            10,
        );

        assert_eq!(
            zones
                .iter()
                .map(|zone| zone.low.as_str())
                .collect::<Vec<_>>(),
            vec!["110", "101"]
        );
    }
}
