use analysis_domain::{Direction, PlanKind};
use backtest_engine::{
    BacktestDiagnosticObservation, BacktestTrade, FeeConfig, SlippageConfig, compute_diagnostics,
    compute_metrics,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use uuid::Uuid;

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn trade(sequence: i64, direction: Direction, gross_r: i64, ambiguous: bool) -> BacktestTrade {
    BacktestTrade {
        trade_id: Uuid::from_u128(10_000 + sequence as u128),
        signal_id: Uuid::from_u128(20_000 + sequence as u128),
        sequence,
        symbol: symbol(),
        plan: PlanKind::TrendBreakout,
        timeframe: Timeframe::M1,
        direction,
        htf_context: None,
        entry_time: sequence * 60_000,
        exit_time: sequence * 60_000 + 120_000,
        entry_price: Decimal::new(100, 0),
        exit_price: Decimal::new(100 + gross_r * 10, 0),
        gross_r: Decimal::new(gross_r, 0),
        net_r: Decimal::new(gross_r, 0) - Decimal::new(15, 2),
        mfe_r: Decimal::new(gross_r + 1, 0),
        mae_r: Decimal::new(-1, 0),
        duration_ms: 120_000,
        exit_reason: if gross_r >= 0 {
            "TAKE_PROFIT"
        } else {
            "STOP_LOSS"
        }
        .to_owned(),
        fees_r: Decimal::new(1, 1),
        slippage_r: Decimal::new(5, 2),
        ambiguous_intrabar: ambiguous,
        partial_filled: false,
        realized_r: Decimal::ZERO,
        runner_contribution_r: Some(Decimal::new(gross_r, 0)),
        hard_stop_counterfactual_r: Some(Decimal::NEGATIVE_ONE),
        tp2_execution_edge_r: None,
        initial_risk_price: Some(Decimal::new(10, 0)),
    }
}

#[test]
fn metrics_include_required_return_risk_breakdown_and_period() {
    let fees = FeeConfig {
        per_execution_r: Decimal::new(1, 1),
    };
    let slippage = SlippageConfig {
        per_execution_r: Decimal::new(5, 2),
    };
    let metrics = compute_metrics(
        vec![
            trade(0, Direction::Buy, 2, false),
            trade(1, Direction::Sell, -1, true),
            trade(2, Direction::Buy, 1, false),
        ],
        0,
        240_000,
        5,
        fees,
        slippage,
    );

    assert_eq!(metrics.total_trades, 3);
    assert_eq!(metrics.winning_trades, 2);
    assert_eq!(metrics.losing_trades, 1);
    assert_eq!(metrics.win_rate, Decimal::new(6667, 4));
    assert_eq!(metrics.loss_rate, Decimal::new(3333, 4));
    assert_eq!(metrics.net_return_r, Decimal::new(155, 2));
    assert_eq!(metrics.profit_factor, Decimal::new(23478, 4));
    assert_eq!(metrics.expectancy_r, Decimal::new(5167, 4));
    assert_eq!(metrics.average_r, Decimal::new(5167, 4));
    assert_eq!(metrics.median_r, Decimal::new(85, 2));
    assert_eq!(metrics.max_drawdown_r, Decimal::new(115, 2));
    assert!(metrics.sharpe > Decimal::ZERO);
    assert_eq!(metrics.consecutive_wins, 1);
    assert_eq!(metrics.consecutive_losses, 1);
    assert_eq!(metrics.long_trades, 2);
    assert_eq!(metrics.short_trades, 1);
    assert_eq!(metrics.fees_r, Decimal::new(3, 1));
    assert_eq!(metrics.slippage_r, Decimal::new(15, 2));
    assert_eq!(metrics.ambiguous_intrabar_count, 1);
    assert_eq!(metrics.start_time, 0);
    assert_eq!(metrics.end_time, 240_000);
    assert_eq!(metrics.candle_count, 5);
}

#[test]
fn empty_metrics_are_stable() {
    let metrics = compute_metrics(
        Vec::new(),
        0,
        60_000,
        1,
        FeeConfig::default(),
        SlippageConfig::default(),
    );

    assert_eq!(metrics.total_trades, 0);
    assert_eq!(metrics.net_return_r, Decimal::ZERO);
    assert_eq!(metrics.start_time, 0);
    assert_eq!(metrics.end_time, 60_000);
    assert_eq!(metrics.candle_count, 1);
}

#[test]
fn weighted_lifecycle_diagnostics_keep_explicit_denominators_and_counterfactuals() {
    let mut stopped = trade(0, Direction::Buy, -1, false);
    stopped.exit_reason = "STOP_LOSS".to_owned();
    stopped.net_r = Decimal::new(-11, 1);
    stopped.runner_contribution_r = Some(Decimal::NEGATIVE_ONE);

    let mut emergency = trade(1, Direction::Buy, 1, false);
    emergency.exit_reason = "EMERGENCY_EXIT".to_owned();
    emergency.partial_filled = true;
    emergency.realized_r = Decimal::new(5, 1);
    emergency.runner_contribution_r = Some(Decimal::new(2, 1));
    emergency.gross_r = Decimal::new(7, 1);
    emergency.net_r = Decimal::new(55, 2);
    emergency.hard_stop_counterfactual_r = Some(Decimal::ZERO);

    let mut suggest = trade(2, Direction::Sell, 0, false);
    suggest.exit_reason = "SUGGEST_CLOSE".to_owned();
    suggest.gross_r = Decimal::new(-2, 1);
    suggest.net_r = Decimal::new(-35, 2);
    suggest.runner_contribution_r = Some(Decimal::new(-2, 1));

    let observations = vec![
        BacktestDiagnosticObservation {
            trade_id: stopped.trade_id,
            post_stop_mfe_r: Some(Decimal::new(14, 1)),
            near_target_reversal: false,
        },
        BacktestDiagnosticObservation {
            trade_id: emergency.trade_id,
            post_stop_mfe_r: None,
            near_target_reversal: true,
        },
    ];
    let diagnostics = compute_diagnostics(&[stopped, emergency, suggest], &observations);

    assert_eq!(diagnostics.hard_stop_count, 1);
    assert_eq!(diagnostics.hard_stop_frequency, Decimal::new(3333, 4));
    assert_eq!(diagnostics.post_stop_mfe_r, Decimal::new(14, 1));
    assert_eq!(diagnostics.near_target_reversal_count, 1);
    assert_eq!(
        diagnostics.near_target_reversal_frequency,
        Decimal::new(3333, 4)
    );
    assert_eq!(diagnostics.partial_fill_count, 1);
    assert_eq!(diagnostics.partial_fill_frequency, Decimal::new(3333, 4));
    assert_eq!(diagnostics.weighted_net_return_r, Decimal::new(-9, 1));
    assert_eq!(diagnostics.runner_contribution_r, Decimal::new(-1, 0));
    assert_eq!(diagnostics.emergency_exit_count, 1);
    assert_eq!(
        diagnostics.emergency_exit_effectiveness_r,
        Decimal::new(7, 1)
    );
    assert_eq!(diagnostics.suggest_close_count, 1);
    assert_eq!(
        diagnostics.suggest_close_effectiveness_r,
        Decimal::new(8, 1)
    );
}

#[test]
fn legacy_trades_default_to_full_position_diagnostics() {
    let json = serde_json::to_value(trade(0, Direction::Buy, 1, false)).unwrap();
    let mut object = json.as_object().unwrap().clone();
    for field in [
        "partialFilled",
        "realizedR",
        "runnerContributionR",
        "hardStopCounterfactualR",
        "tp2ExecutionEdgeR",
        "initialRiskPrice",
    ] {
        object.remove(field);
    }
    let legacy: BacktestTrade = serde_json::from_value(object.into()).unwrap();
    let diagnostics = compute_diagnostics(&[legacy], &[]);

    assert_eq!(diagnostics.partial_fill_count, 0);
    assert_eq!(diagnostics.runner_contribution_r, Decimal::ONE);
    assert_eq!(diagnostics.weighted_net_return_r, Decimal::new(85, 2));
}
