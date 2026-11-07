use execution_domain::MarketCursor;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactReplayPlan {
    pub cursor: MarketCursor,
    pub from_id: i64,
    pub limit: u16,
}

impl ExactReplayPlan {
    pub fn from_cursor(cursor: MarketCursor, limit: u16) -> Self {
        Self {
            from_id: cursor.last_processed_agg_trade_id + 1,
            cursor,
            limit,
        }
    }
}
