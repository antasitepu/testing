use analysis_domain::{Direction, HtfContext, PlanKind};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeeConfig {
    pub per_execution_r: Decimal,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SlippageConfig {
    pub per_execution_r: Decimal,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountSimulationConfig {
    pub starting_equity: Decimal,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BacktestTrade {
    pub trade_id: Uuid,
    pub signal_id: Uuid,
    pub sequence: i64,
    pub symbol: Symbol,
    pub plan: PlanKind,
    pub timeframe: Timeframe,
    pub direction: Direction,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub htf_context: Option<HtfContext>,
    pub entry_time: i64,
    pub exit_time: i64,
    pub entry_price: Decimal,
    pub exit_price: Decimal,
    pub gross_r: Decimal,
    pub net_r: Decimal,
    pub mfe_r: Decimal,
    pub mae_r: Decimal,
    pub duration_ms: i64,
    pub exit_reason: String,
    pub fees_r: Decimal,
    pub slippage_r: Decimal,
    pub ambiguous_intrabar: bool,
    #[serde(default)]
    pub partial_filled: bool,
    #[serde(default)]
    pub realized_r: Decimal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_contribution_r: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_stop_counterfactual_r: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tp2_execution_edge_r: Option<Decimal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_risk_price: Option<Decimal>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BacktestDiagnosticObservation {
    pub trade_id: Uuid,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_stop_mfe_r: Option<Decimal>,
    #[serde(default)]
    pub near_target_reversal: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BacktestDiagnostics {
    pub hard_stop_count: u64,
    pub hard_stop_frequency: Decimal,
    pub post_stop_mfe_r: Decimal,
    pub near_target_reversal_count: u64,
    pub near_target_reversal_frequency: Decimal,
    pub partial_fill_count: u64,
    pub partial_fill_frequency: Decimal,
    pub weighted_net_return_r: Decimal,
    pub runner_contribution_r: Decimal,
    pub emergency_exit_count: u64,
    pub emergency_exit_effectiveness_r: Decimal,
    pub suggest_close_count: u64,
    pub suggest_close_effectiveness_r: Decimal,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BacktestMetrics {
    pub total_trades: u64,
    pub winning_trades: u64,
    pub losing_trades: u64,
    pub win_rate: Decimal,
    pub loss_rate: Decimal,
    pub net_return_r: Decimal,
    pub profit_factor: Decimal,
    pub expectancy_r: Decimal,
    pub average_r: Decimal,
    pub median_r: Decimal,
    pub max_drawdown_r: Decimal,
    pub sharpe: Decimal,
    pub consecutive_wins: u64,
    pub consecutive_losses: u64,
    pub long_trades: u64,
    pub short_trades: u64,
    pub fees_r: Decimal,
    pub slippage_r: Decimal,
    pub ambiguous_intrabar_count: u64,
    pub start_time: i64,
    pub end_time: i64,
    pub candle_count: u64,
}

pub fn compute_metrics(
    mut trades: Vec<BacktestTrade>,
    start_time: i64,
    end_time: i64,
    candle_count: u64,
    _fees: FeeConfig,
    _slippage: SlippageConfig,
) -> BacktestMetrics {
    trades.sort_by_key(|trade| trade.sequence);
    let total_trades = trades.len() as u64;
    let winning_trades = trades
        .iter()
        .filter(|trade| trade.net_r > Decimal::ZERO)
        .count() as u64;
    let losing_trades = trades
        .iter()
        .filter(|trade| trade.net_r < Decimal::ZERO)
        .count() as u64;
    let net_return_r = sum(trades.iter().map(|trade| trade.net_r));
    let gross_wins = sum(trades
        .iter()
        .filter(|trade| trade.net_r > Decimal::ZERO)
        .map(|trade| trade.net_r));
    let gross_losses = sum(trades
        .iter()
        .filter(|trade| trade.net_r < Decimal::ZERO)
        .map(|trade| trade.net_r.abs()));
    let (consecutive_wins, consecutive_losses) = streaks(&trades);
    BacktestMetrics {
        total_trades,
        winning_trades,
        losing_trades,
        win_rate: ratio(winning_trades, total_trades),
        loss_rate: ratio(losing_trades, total_trades),
        net_return_r,
        profit_factor: if gross_losses.is_zero() {
            Decimal::ZERO
        } else {
            round4(gross_wins / gross_losses)
        },
        expectancy_r: ratio_decimal(net_return_r, total_trades),
        average_r: ratio_decimal(net_return_r, total_trades),
        median_r: median(trades.iter().map(|trade| trade.net_r).collect()),
        max_drawdown_r: max_drawdown(&trades),
        sharpe: sharpe(&trades),
        consecutive_wins,
        consecutive_losses,
        long_trades: trades
            .iter()
            .filter(|trade| trade.direction == Direction::Buy)
            .count() as u64,
        short_trades: trades
            .iter()
            .filter(|trade| trade.direction == Direction::Sell)
            .count() as u64,
        fees_r: sum(trades.iter().map(|trade| trade.fees_r)),
        slippage_r: sum(trades.iter().map(|trade| trade.slippage_r)),
        ambiguous_intrabar_count: trades
            .iter()
            .filter(|trade| trade.ambiguous_intrabar)
            .count() as u64,
        start_time,
        end_time,
        candle_count,
    }
}

pub fn compute_diagnostics(
    trades: &[BacktestTrade],
    observations: &[BacktestDiagnosticObservation],
) -> BacktestDiagnostics {
    let total_trades = trades.len() as u64;
    let observations = observations
        .iter()
        .map(|observation| (observation.trade_id, observation))
        .collect::<HashMap<_, _>>();
    let hard_stop_count = trades
        .iter()
        .filter(|trade| trade.exit_reason == "STOP_LOSS" && !trade.partial_filled)
        .count() as u64;
    let partial_fill_count = trades.iter().filter(|trade| trade.partial_filled).count() as u64;
    let near_target_reversal_count = trades
        .iter()
        .filter(|trade| {
            observations
                .get(&trade.trade_id)
                .is_some_and(|observation| observation.near_target_reversal)
        })
        .count() as u64;
    let post_stop_values = trades.iter().filter_map(|trade| {
        observations
            .get(&trade.trade_id)
            .and_then(|observation| observation.post_stop_mfe_r)
    });
    let post_stop_values = post_stop_values.collect::<Vec<_>>();
    let weighted_net_return_r = sum(trades.iter().map(|trade| trade.net_r));
    let runner_contribution_r = sum(trades
        .iter()
        .map(|trade| trade.runner_contribution_r.unwrap_or(trade.gross_r)));
    let emergency = exit_effectiveness(trades, "EMERGENCY_EXIT");
    let suggest = exit_effectiveness(trades, "SUGGEST_CLOSE");

    BacktestDiagnostics {
        hard_stop_count,
        hard_stop_frequency: ratio(hard_stop_count, total_trades),
        post_stop_mfe_r: average(&post_stop_values),
        near_target_reversal_count,
        near_target_reversal_frequency: ratio(near_target_reversal_count, total_trades),
        partial_fill_count,
        partial_fill_frequency: ratio(partial_fill_count, total_trades),
        weighted_net_return_r,
        runner_contribution_r,
        emergency_exit_count: emergency.0,
        emergency_exit_effectiveness_r: emergency.1,
        suggest_close_count: suggest.0,
        suggest_close_effectiveness_r: suggest.1,
    }
}

fn exit_effectiveness(trades: &[BacktestTrade], reason: &str) -> (u64, Decimal) {
    let values = trades
        .iter()
        .filter(|trade| trade.exit_reason == reason)
        .map(|trade| {
            trade.gross_r
                - trade
                    .hard_stop_counterfactual_r
                    .unwrap_or(Decimal::NEGATIVE_ONE)
        })
        .collect::<Vec<_>>();
    (values.len() as u64, average(&values))
}

fn average(values: &[Decimal]) -> Decimal {
    if values.is_empty() {
        Decimal::ZERO
    } else {
        round4(sum(values.iter().copied()) / Decimal::from(values.len()))
    }
}

fn sum(values: impl Iterator<Item = Decimal>) -> Decimal {
    values.fold(Decimal::ZERO, |accumulator, value| accumulator + value)
}

fn ratio(part: u64, total: u64) -> Decimal {
    if total == 0 {
        Decimal::ZERO
    } else {
        round4(Decimal::from(part) / Decimal::from(total))
    }
}

fn ratio_decimal(value: Decimal, total: u64) -> Decimal {
    if total == 0 {
        Decimal::ZERO
    } else {
        round4(value / Decimal::from(total))
    }
}

fn round4(value: Decimal) -> Decimal {
    value.round_dp(4).normalize()
}

fn median(mut values: Vec<Decimal>) -> Decimal {
    if values.is_empty() {
        return Decimal::ZERO;
    }
    values.sort();
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        round4((values[middle - 1] + values[middle]) / Decimal::TWO)
    } else {
        values[middle]
    }
}

fn max_drawdown(trades: &[BacktestTrade]) -> Decimal {
    let mut equity = Decimal::ZERO;
    let mut peak = Decimal::ZERO;
    let mut drawdown = Decimal::ZERO;
    for trade in trades {
        equity += trade.net_r;
        if equity > peak {
            peak = equity;
        }
        let current = peak - equity;
        if current > drawdown {
            drawdown = current;
        }
    }
    drawdown
}

fn sharpe(trades: &[BacktestTrade]) -> Decimal {
    if trades.len() < 2 {
        return Decimal::ZERO;
    }
    let values: Vec<f64> = trades
        .iter()
        .filter_map(|trade| trade.net_r.to_f64())
        .collect();
    if values.len() < 2 {
        return Decimal::ZERO;
    }
    let average = values.iter().sum::<f64>() / values.len() as f64;
    let variance = values
        .iter()
        .map(|value| (value - average).powi(2))
        .sum::<f64>()
        / (values.len() - 1) as f64;
    if variance == 0.0 {
        return Decimal::ZERO;
    }
    Decimal::from_f64(average / variance.sqrt())
        .unwrap_or(Decimal::ZERO)
        .round_dp(4)
        .normalize()
}

fn streaks(trades: &[BacktestTrade]) -> (u64, u64) {
    let mut best_wins = 0;
    let mut best_losses = 0;
    let mut wins = 0;
    let mut losses = 0;
    for trade in trades {
        if trade.net_r > Decimal::ZERO {
            wins += 1;
            losses = 0;
        } else if trade.net_r < Decimal::ZERO {
            losses += 1;
            wins = 0;
        } else {
            wins = 0;
            losses = 0;
        }
        best_wins = best_wins.max(wins);
        best_losses = best_losses.max(losses);
    }
    (best_wins, best_losses)
}
