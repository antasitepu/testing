use crate::{StorageError, parse_market_status};
use market_domain::{Market, Symbol};
use sqlx::{PgPool, Row};

#[derive(Clone)]
pub struct MarketRepository {
    pool: PgPool,
}

impl MarketRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn upsert(&self, market: &Market) -> Result<(), StorageError> {
        sqlx::query(
            r#"
            INSERT INTO markets (
                symbol, enabled, status, price_tick, onboard_time, session_market
            )
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (symbol) DO UPDATE SET
                enabled = EXCLUDED.enabled,
                status = EXCLUDED.status,
                price_tick = EXCLUDED.price_tick,
                onboard_time = EXCLUDED.onboard_time,
                session_market = EXCLUDED.session_market,
                updated_at = NOW()
            "#,
        )
        .bind(market.symbol.as_str())
        .bind(market.enabled)
        .bind(market_status_text(market.status))
        .bind(market.price_tick)
        .bind(market.onboard_time)
        .bind(&market.session_market)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn load(&self, symbol: &Symbol) -> Result<Option<Market>, StorageError> {
        let row = sqlx::query(
            r#"
            SELECT symbol, enabled, status, price_tick, onboard_time, session_market
            FROM markets
            WHERE symbol = $1
            "#,
        )
        .bind(symbol.as_str())
        .fetch_optional(&self.pool)
        .await?;
        row.map(row_to_market).transpose()
    }

    pub async fn enabled(&self) -> Result<Vec<Market>, StorageError> {
        sqlx::query(
            r#"
            SELECT symbol, enabled, status, price_tick, onboard_time, session_market
            FROM markets
            WHERE enabled = TRUE
            ORDER BY symbol
            "#,
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .map(row_to_market)
        .collect()
    }
}

fn row_to_market(row: sqlx::postgres::PgRow) -> Result<Market, StorageError> {
    let symbol_text: String = row.try_get("symbol")?;
    Market::new(
        Symbol::new(symbol_text).map_err(|error| StorageError::InvalidData(error.to_string()))?,
        row.try_get("enabled")?,
        parse_market_status(row.try_get("status")?)?,
        row.try_get("price_tick")?,
        row.try_get("onboard_time")?,
        row.try_get::<String, _>("session_market")?,
    )
    .map_err(|error| StorageError::InvalidData(error.to_string()))
}

fn market_status_text(status: market_domain::MarketStatus) -> &'static str {
    match status {
        market_domain::MarketStatus::Trading => "TRADING",
        market_domain::MarketStatus::Halted => "HALTED",
        market_domain::MarketStatus::Unknown => "UNKNOWN",
    }
}
