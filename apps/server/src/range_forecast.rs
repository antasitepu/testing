//! Display-only range forecast: a frozen `vol-v1` HAR_RANGE_SEASONAL fit that
//! estimates the typical high-low range of the coming 24 bars. It never feeds
//! plans, signals or execution. See
//! `docs/superpowers/specs/2026-11-08-gold-range-forecast-benchmark-design.md`.

use analysis_core::calculate_atr;
use analysis_core::range_forecast::{HORIZON_BARS, Lookbacks, RangeModel};
use market_domain::{Candle, MarketSession, Symbol, Timeframe};
use market_service::{CandleStore, MarketStore, ServiceError, SessionStore, expected_open_times};
use serde::Serialize;
use std::{collections::HashSet, path::Path, sync::Arc};

/// Raw candles read per request: a month of tradable 15m bars plus closed days.
const RAW_LIMIT: i64 = 4_000;
/// The plans' gap rule: no missing expected bar within the last 200 bars.
const GAP_LOOKBACK_BARS: i64 = 200;
/// How far ahead the next 24 tradable bars are looked for (covers a long weekend).
const FUTURE_SEARCH_MS: i64 = 7 * 86_400_000;

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RangeForecastDto {
    pub symbol: String,
    pub timeframe: Option<Timeframe>,
    pub horizon_bars: usize,
    /// Open of the first bar of the horizon.
    pub from_time: Option<i64>,
    /// Close of the last bar of the horizon.
    pub until_time: Option<i64>,
    /// Typical high-low range over the horizon, in price.
    pub range: Option<String>,
    /// What calibrated ATR14 expects for the same horizon.
    pub atr_range: Option<String>,
    pub model: Option<String>,
    /// Why no forecast is shown.
    pub reason: Option<String>,
}

impl RangeForecastDto {
    pub fn unavailable(symbol: &Symbol, reason: &str) -> Self {
        Self {
            symbol: symbol.to_string(),
            timeframe: None,
            horizon_bars: HORIZON_BARS,
            from_time: None,
            until_time: None,
            range: None,
            atr_range: None,
            model: None,
            reason: Some(reason.to_owned()),
        }
    }
}

/// A forecast for the horizon starting at `from_time`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HorizonForecast {
    pub from_time: i64,
    pub until_time: i64,
    pub range: f64,
    pub atr_range: f64,
}

pub struct RangeForecaster {
    model: RangeModel,
    timeframe: Timeframe,
    sessions: Arc<dyn SessionStore>,
}

impl RangeForecaster {
    pub fn new(model: RangeModel, sessions: Arc<dyn SessionStore>) -> Result<Self, String> {
        let timeframe = model.validated_timeframe()?;
        Ok(Self {
            model,
            timeframe,
            sessions,
        })
    }

    pub fn from_file(path: &Path, sessions: Arc<dyn SessionStore>) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|error| format!("read range forecast model {}: {error}", path.display()))?;
        let model: RangeModel = serde_json::from_str(&text)
            .map_err(|error| format!("parse range forecast model {}: {error}", path.display()))?;
        Self::new(model, sessions)
    }

    pub async fn forecast(
        &self,
        markets: &dyn MarketStore,
        candles: &dyn CandleStore,
        symbol: &Symbol,
        now_ms: i64,
    ) -> Result<RangeForecastDto, ServiceError> {
        if symbol.as_str() != self.model.symbol {
            return Ok(RangeForecastDto::unavailable(
                symbol,
                "no range forecast model for this symbol",
            ));
        }
        let Some(market) = markets.load_market(symbol).await? else {
            return Ok(RangeForecastDto::unavailable(symbol, "unknown market"));
        };
        let raw = candles
            .latest(symbol, self.timeframe, None, RAW_LIMIT)
            .await?;
        let Some(first_open) = raw.first().map(|candle| candle.open_time) else {
            return Ok(RangeForecastDto::unavailable(symbol, "no candles"));
        };
        let current_open = now_ms - now_ms.rem_euclid(self.timeframe.duration_ms());
        let sessions = self
            .sessions
            .between(
                &market.session_market,
                first_open,
                current_open.saturating_add(FUTURE_SEARCH_MS),
            )
            .await?;
        let mut dto = RangeForecastDto::unavailable(symbol, "");
        dto.timeframe = Some(self.timeframe);
        dto.model = Some(format!("{}/{}", self.model.vol_version, self.model.model));
        match forecast_horizon(
            &self.model,
            self.timeframe,
            raw,
            &sessions,
            market.onboard_time,
            now_ms,
        ) {
            Ok(forecast) => {
                dto.from_time = Some(forecast.from_time);
                dto.until_time = Some(forecast.until_time);
                dto.range = Some(format!("{:.2}", forecast.range));
                dto.atr_range = Some(format!("{:.2}", forecast.atr_range));
                dto.reason = None;
            }
            Err(reason) => dto.reason = Some(reason.to_owned()),
        }
        Ok(dto)
    }
}

/// The forecast for the 24 tradable bars from the bar forming at `now_ms`, from
/// raw candles in ascending order. Readiness follows the research harness: no
/// missing expected bar in the last 200 bars and a month of session-clean history.
pub fn forecast_horizon(
    model: &RangeModel,
    timeframe: Timeframe,
    raw: Vec<Candle>,
    sessions: &[MarketSession],
    onboard_time: i64,
    now_ms: i64,
) -> Result<HorizonForecast, &'static str> {
    let duration = timeframe.duration_ms();
    let current_open = now_ms - now_ms.rem_euclid(duration);
    let closed: Vec<Candle> = raw
        .into_iter()
        .filter(|candle| candle.close_time < current_open)
        .collect();
    let first_open = closed.first().ok_or("no closed candles")?.open_time;

    let present: HashSet<i64> = closed.iter().map(|candle| candle.open_time).collect();
    let gap_start = first_open.max(current_open - duration * GAP_LOOKBACK_BARS);
    if expected_open_times(timeframe, gap_start, current_open, sessions)
        .iter()
        .any(|open| !present.contains(open))
    {
        return Err("recent candles are missing");
    }

    let history =
        crate::runtime_analysis_candles::filter_runtime_candles(closed, sessions, onboard_time)
            .map_err(|_| "session coverage is unknown")?;
    if history.len() < Lookbacks::of(timeframe).month + 1 {
        return Err("less than a month of history");
    }
    let atr = calculate_atr(&history, 14)
        .ok()
        .and_then(|output| output.values.last().copied().flatten())
        .ok_or("ATR is not ready")?;

    let future_opens: Vec<i64> = expected_open_times(
        timeframe,
        current_open,
        current_open.saturating_add(FUTURE_SEARCH_MS),
        sessions,
    )
    .into_iter()
    .take(model.horizon_bars)
    .collect();
    if future_opens.len() < model.horizon_bars {
        return Err("upcoming sessions are not known yet");
    }
    let forecast = model
        .forecast(&history, atr, &future_opens, timeframe)
        .ok_or("forecast inputs are not positive")?;
    Ok(HorizonForecast {
        from_time: future_opens[0],
        until_time: future_opens[future_opens.len() - 1] + duration - 1,
        range: forecast.range,
        atr_range: forecast.atr_range,
    })
}
