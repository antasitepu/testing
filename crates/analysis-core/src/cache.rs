use crate::SnapshotRequest;
use analysis_domain::{CanonicalMarketSnapshot, PrimitiveVersion};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use std::collections::HashMap;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct SnapshotCacheKey {
    pub symbol: Symbol,
    pub timeframe: Timeframe,
    pub price_tick: Decimal,
    pub evaluated_closed_candle_time: i64,
    pub primitive_versions: Vec<PrimitiveVersion>,
}

impl SnapshotCacheKey {
    pub fn from_request(request: &SnapshotRequest) -> Self {
        let mut primitive_versions = request.primitive_versions.clone();
        primitive_versions.sort();
        Self {
            symbol: request.symbol.clone(),
            timeframe: request.target_timeframe,
            price_tick: request.price_tick,
            evaluated_closed_candle_time: request.evaluated_closed_candle_time,
            primitive_versions,
        }
    }
}

#[derive(Default)]
pub struct SnapshotCache {
    values: HashMap<SnapshotCacheKey, CanonicalMarketSnapshot>,
}

impl SnapshotCache {
    pub fn insert(&mut self, key: SnapshotCacheKey, snapshot: CanonicalMarketSnapshot) {
        self.values.insert(key, snapshot);
    }

    pub fn get(&self, key: &SnapshotCacheKey) -> Option<&CanonicalMarketSnapshot> {
        self.values.get(key)
    }

    pub fn clear(&mut self) {
        self.values.clear();
    }
}
