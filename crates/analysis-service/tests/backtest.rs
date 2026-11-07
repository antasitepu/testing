use analysis_domain::PlanKind;
use analysis_service::{BacktestRunInput, BacktestRunRecord};
use backtest_engine::{FeeConfig, SlippageConfig};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use uuid::Uuid;

fn input(release_id: Uuid) -> BacktestRunInput {
    BacktestRunInput {
        symbol: Symbol::new("XAUUSDT").unwrap(),
        plan: PlanKind::TrendBreakout,
        target_timeframe: Timeframe::M1,
        start_time: 0,
        end_time: 60_000,
        release_id,
        algorithm_version: "trend-breakout-v1".to_owned(),
        parameter_version: "trend-breakout-1m-v1".to_owned(),
        fees: FeeConfig {
            per_execution_r: Decimal::new(1, 2),
        },
        slippage: SlippageConfig {
            per_execution_r: Decimal::new(1, 2),
        },
        account: None,
        code_commit_hash: "abc1234".to_owned(),
    }
}

#[test]
fn typed_backtest_input_creates_immutable_persistence_record() {
    let run_id = Uuid::from_u128(42);
    let release_id = Uuid::from_u128(7);
    let input = input(release_id);
    let record = BacktestRunRecord::new(run_id, input.clone()).unwrap();

    assert_eq!(record.run_id, run_id);
    assert_eq!(record.code_commit_hash, "abc1234");
    assert_eq!(record.input, input);
    assert_eq!(record.input_json["releaseId"], release_id.to_string());
    assert_eq!(record.input_json["codeCommitHash"], "abc1234");
}
