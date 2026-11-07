use analysis_domain::MaterializedProfile;
use analysis_domain::{
    CanonicalMarketSnapshot, ConfirmedFact, Direction, DirectiveKind, ExecutionInstruction, FactId,
    FactKind, FactLifecycle, HtfAlignment, HtfContext, PlanFeedback, PlanKind, PlanParameters,
    PlanProfileKey, PrimitiveVersion, Readiness, SignalDefinition, TimeframeFacts, TradeState,
    VersionIdentity,
};
use analysis_domain::{MeanReversionParameters, PreEntryValidation};
use analysis_plans::{
    MeanReversionEvaluator, PlanEvaluator, PlanRuntimeState, PreEntryRejection,
    validate_mean_reversion_entry,
};
use market_domain::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

fn symbol() -> Symbol {
    Symbol::new("XAUUSDT").unwrap()
}

fn id(kind: FactKind, ordinal: u32, confirmed_at: i64) -> FactId {
    FactId {
        symbol: symbol(),
        timeframe: Timeframe::M1,
        kind,
        confirmed_at,
        ordinal,
        algorithm_version: "fixture-v1".to_owned(),
        parameter_version: "fixture-v1".to_owned(),
    }
}

fn fact<T: Serialize>(id: FactId, value: T) -> ConfirmedFact<serde_json::Value> {
    ConfirmedFact {
        id,
        value: serde_json::to_value(value).unwrap(),
        lifecycle: FactLifecycle::Confirmed,
    }
}

fn profile() -> MaterializedProfile {
    MaterializedProfile {
        key: PlanProfileKey {
            symbol: symbol(),
            plan: PlanKind::MeanReversion,
            target_timeframe: Timeframe::M1,
        },
        enabled: true,
        profit_protection: analysis_domain::ProfitProtectionParameters::default(),
        position_management: analysis_domain::PositionManagementParameters::default(),
        versions: VersionIdentity::new(Uuid::now_v7(), "mean-v1", "mean-1m-v1").unwrap(),
        parameters: PlanParameters::MeanReversion(MeanReversionParameters {
            bollinger_period: 20,
            bollinger_deviation: Decimal::TWO,
            zscore_period: 20,
            extreme_zscore: Decimal::new(2, 0),
            adx_period: 14,
            maximum_adx: Decimal::new(25, 0),
            maximum_bandwidth: Decimal::new(3, 2),
            atr_period: 14,
            atr_buffer_multiplier: Decimal::new(2, 1),
            minimum_rr: Decimal::ONE,
            max_bars_valid: 5,
        }),
        required_timeframes: Vec::new(),
        minimum_history_bars: 100,
        warmup_bars: 30,
    }
}

fn band(
    ordinal: u32,
    confirmed_at: i64,
    close: Decimal,
    low: Decimal,
    high: Decimal,
    zscore: Decimal,
) -> ConfirmedFact<serde_json::Value> {
    fact(
        id(FactKind::Bollinger, ordinal, confirmed_at),
        json!({
            "period": 20,
            "deviationMultiplier": "2",
            "upper": "4100",
            "middle": "4050",
            "lower": "4000",
            "deviation": "25",
            "bandwidth": "0.02",
            "zscore": zscore.to_string(),
            "close": close.to_string(),
            "low": low.to_string(),
            "high": high.to_string()
        }),
    )
}

fn snapshot(direction: Direction) -> CanonicalMarketSnapshot {
    let observations = if direction == Direction::Buy {
        vec![
            band(
                1,
                60_000,
                Decimal::new(3_980, 0),
                Decimal::new(3_970, 0),
                Decimal::new(3_990, 0),
                Decimal::new(-3, 0),
            ),
            band(
                2,
                120_000,
                Decimal::new(3_975, 0),
                Decimal::new(3_960, 0),
                Decimal::new(3_985, 0),
                Decimal::new(-35, 1),
            ),
            band(
                3,
                180_000,
                Decimal::new(4_001, 0),
                Decimal::new(3_990, 0),
                Decimal::new(4_010, 0),
                Decimal::new(-5, 1),
            ),
        ]
    } else {
        vec![
            band(
                1,
                60_000,
                Decimal::new(4_120, 0),
                Decimal::new(4_110, 0),
                Decimal::new(4_130, 0),
                Decimal::new(3, 0),
            ),
            band(
                2,
                120_000,
                Decimal::new(4_125, 0),
                Decimal::new(4_115, 0),
                Decimal::new(4_140, 0),
                Decimal::new(35, 1),
            ),
            band(
                3,
                180_000,
                Decimal::new(4_099, 0),
                Decimal::new(4_090, 0),
                Decimal::new(4_110, 0),
                Decimal::new(5, 1),
            ),
        ]
    };
    let mut facts = observations;
    facts.push(fact(id(FactKind::Adx, 4, 180_000), json!({"value": "20"})));
    facts.push(fact(id(FactKind::Atr, 5, 180_000), json!({"value": "10"})));
    facts.push(fact(
        id(FactKind::MarketClose, 6, 180_000),
        json!({"value": if direction == Direction::Buy { "4001" } else { "4099" }}),
    ));
    CanonicalMarketSnapshot {
        symbol: symbol(),
        target_timeframe: Timeframe::M1,
        price_tick: Decimal::new(1, 1),
        evaluated_closed_candle_time: 239_999,
        readiness: Readiness::Ready,
        target: TimeframeFacts {
            timeframe: Timeframe::M1,
            evaluated_closed_candle_time: 239_999,
            facts,
        },
        higher_timeframes: Vec::new(),
        primitive_versions: vec![PrimitiveVersion {
            timeframe: Timeframe::M1,
            primitive: "fixture".to_owned(),
            algorithm_version: "fixture-v1".to_owned(),
            parameter_version: "fixture-v1".to_owned(),
        }],
    }
}

fn assert_execution_plan(signal: &SignalDefinition, thesis: Decimal) {
    let plan = signal.execution_plan.as_ref().expect("execution plan");
    assert_eq!(plan.thesis_invalidation, thesis);
    assert_eq!(plan.hard_stop, signal.stop_loss);
    assert_eq!(plan.tp1.trigger_r, Decimal::ONE);
    assert_eq!(plan.tp1.fraction, Decimal::new(5, 1));
    assert!(plan.tp2_zone.low <= signal.primary_target);
    assert!(plan.tp2_zone.high >= signal.primary_target);
    match signal.direction {
        Direction::Buy => {
            assert!(plan.hard_stop < plan.thesis_invalidation);
            assert!(plan.tp2_zone.low < signal.primary_target);
        }
        Direction::Sell => {
            assert!(plan.hard_stop > plan.thesis_invalidation);
            assert!(plan.tp2_zone.high > signal.primary_target);
        }
        Direction::None => unreachable!(),
    }
    assert_eq!(signal.validate_execution_geometry(), Ok(()));
}

fn push_market_close(snapshot: &mut CanonicalMarketSnapshot, confirmed_at: i64, close: Decimal) {
    snapshot.target.facts.push(fact(
        id(FactKind::MarketClose, 200, confirmed_at),
        json!({"value": close.to_string()}),
    ));
    snapshot.evaluated_closed_candle_time = confirmed_at + 59_999;
    snapshot.target.evaluated_closed_candle_time = snapshot.evaluated_closed_candle_time;
}

#[test]
fn full_extreme_sequence_freezes_mean_target_and_accumulated_stop() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = profile();
        let signal = MeanReversionEvaluator
            .evaluate(
                &profile,
                &snapshot(direction),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap()
            .signal
            .unwrap();
        assert_eq!(signal.direction, direction);
        assert_eq!(signal.execution_instruction, ExecutionInstruction::NextOpen);
        assert_eq!(signal.primary_target, Decimal::new(4_050, 0));
        assert_eq!(
            signal.stop_loss,
            if direction == Direction::Buy {
                Decimal::new(3_958, 0)
            } else {
                Decimal::new(4_142, 0)
            }
        );
        assert!(signal.risk_reward >= Decimal::ONE);
        assert_eq!(
            signal.pre_entry_validation,
            Some(PreEntryValidation {
                minimum_rr: Decimal::ONE,
                reject_if_target_crossed: true,
                reject_if_stop_crossed: true,
            })
        );
    }
}

#[test]
fn execution_plan_uses_mean_accumulated_extreme_symmetrically() {
    for (direction, thesis) in [
        (Direction::Buy, Decimal::new(3_960, 0)),
        (Direction::Sell, Decimal::new(4_140, 0)),
    ] {
        let profile = profile();
        let signal = MeanReversionEvaluator
            .evaluate(
                &profile,
                &snapshot(direction),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap()
            .signal
            .unwrap();
        assert_execution_plan(&signal, thesis);
    }
}

#[test]
fn signal_captures_opposite_band_as_immutable_extension_target() {
    for (direction, expected) in [
        (Direction::Buy, Decimal::new(4_100, 0)),
        (Direction::Sell, Decimal::new(4_000, 0)),
    ] {
        let profile = profile();
        let signal = MeanReversionEvaluator
            .evaluate(
                &profile,
                &snapshot(direction),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap()
            .signal
            .unwrap();
        assert!(
            signal
                .reference_levels
                .iter()
                .any(|level| { level.kind == "TP2_EXTENSION_TARGET" && level.price == expected })
        );
    }
}

#[test]
fn runner_emits_buffered_move_target_from_immutable_mean_band() {
    let profile = profile();
    let initial = snapshot(Direction::Buy);
    let created = MeanReversionEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let mut state = created.next_state;
    state.active_signal.as_mut().unwrap().htf_context = Some(HtfContext {
        higher_timeframe: None,
        direction: Direction::None,
        alignment: HtfAlignment::Neutral,
        source: "NEUTRAL".to_owned(),
    });
    let signal = state.active_signal.as_ref().unwrap().clone();
    let primary_target = signal.primary_target;
    let trade_id = Uuid::now_v7();
    let mut progress = initial;
    push_market_close(&mut progress, 240_000, Decimal::new(4_048, 0));
    let feedback = [
        PlanFeedback::ActualEntry {
            signal_id: signal.signal_id,
            virtual_trade_id: trade_id,
            time: 200_000,
            price: Decimal::new(4_002, 0),
        },
        PlanFeedback::PartialTargetFilled {
            signal_id: signal.signal_id,
            virtual_trade_id: trade_id,
            time: 220_000,
            price: Decimal::new(4_046, 0),
            realized_fraction: Decimal::new(5, 1),
            remaining_fraction: Decimal::new(5, 1),
        },
    ];

    let evaluation = MeanReversionEvaluator
        .evaluate(&profile, &progress, &state, &feedback)
        .unwrap();

    assert_eq!(evaluation.directives.len(), 1);
    let directive = &evaluation.directives[0];
    assert_eq!(directive.kind, DirectiveKind::MoveTarget);
    assert_eq!(directive.reason, "MEAN_SHARED_TARGET_EXTENSION");
    assert_eq!(directive.price, Some(Decimal::new(4_099, 0)));
    assert_eq!(directive.algorithm_version, signal.algorithm_version);
    assert_eq!(directive.parameter_version, signal.parameter_version);
    assert_eq!(
        evaluation
            .next_state
            .active_signal
            .as_ref()
            .unwrap()
            .primary_target,
        primary_target
    );
}

#[test]
fn execution_plan_uses_minimum_tick_stop_buffer_when_it_is_wider() {
    for (direction, thesis) in [
        (Direction::Buy, Decimal::new(3_960, 0)),
        (Direction::Sell, Decimal::new(4_140, 0)),
    ] {
        let mut profile = profile();
        profile.position_management.minimum_buffer_ticks = 50;
        let signal = MeanReversionEvaluator
            .evaluate(
                &profile,
                &snapshot(direction),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap()
            .signal
            .unwrap();
        let plan = signal.execution_plan.as_ref().unwrap();
        assert_eq!((plan.hard_stop - thesis).abs(), Decimal::new(5, 0));
        let entry = signal.projected_entry.unwrap();
        let actual_rr =
            ((signal.primary_target - entry).abs() / (entry - signal.stop_loss).abs()).round_dp(6);
        assert_eq!(signal.risk_reward, actual_rr);
        assert!(signal.risk_reward >= Decimal::ONE);
    }
}

#[test]
fn regime_rr_and_setup_expiry_block_signal() {
    let base_profile = profile();
    let mut bad_regime = snapshot(Direction::Buy);
    bad_regime
        .target
        .facts
        .iter_mut()
        .find(|fact| fact.id.kind == FactKind::Adx)
        .unwrap()
        .value["value"] = json!("30");
    let mut expired = snapshot(Direction::Buy);
    expired.target.facts[2].id.confirmed_at = 500_000;
    expired.evaluated_closed_candle_time = 559_999;
    let mut bad_rr = profile();
    let PlanParameters::MeanReversion(parameters) = &mut bad_rr.parameters else {
        unreachable!()
    };
    parameters.minimum_rr = Decimal::new(2, 0);
    assert!(
        MeanReversionEvaluator
            .evaluate(
                &base_profile,
                &bad_regime,
                &PlanRuntimeState::neutral(&base_profile),
                &[],
            )
            .unwrap()
            .signal
            .is_none()
    );
    assert!(
        MeanReversionEvaluator
            .evaluate(
                &base_profile,
                &expired,
                &PlanRuntimeState::neutral(&base_profile),
                &[],
            )
            .unwrap()
            .signal
            .is_none()
    );
    assert!(
        MeanReversionEvaluator
            .evaluate(
                &bad_rr,
                &snapshot(Direction::Buy),
                &PlanRuntimeState::neutral(&bad_rr),
                &[],
            )
            .unwrap()
            .signal
            .is_none()
    );
}

#[test]
fn actual_open_validation_rejects_target_stop_and_rr_without_moving_levels() {
    let profile = profile();
    let signal = MeanReversionEvaluator
        .evaluate(
            &profile,
            &snapshot(Direction::Buy),
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap()
        .signal
        .unwrap();
    assert!(validate_mean_reversion_entry(&signal, Decimal::new(4_002, 0)).is_ok());
    assert_eq!(
        validate_mean_reversion_entry(&signal, Decimal::new(4_051, 0)),
        Err(PreEntryRejection::TargetCrossed)
    );
    assert_eq!(
        validate_mean_reversion_entry(&signal, Decimal::new(3_958, 0)),
        Err(PreEntryRejection::StopCrossed)
    );
    assert_eq!(
        validate_mean_reversion_entry(&signal, Decimal::new(4_030, 0)),
        Err(PreEntryRejection::InsufficientRiskReward)
    );
    assert_eq!(signal.primary_target, Decimal::new(4_050, 0));
}

#[test]
fn runner_health_mean_regime_break_emits_emergency_exit() {
    let profile = profile();
    let initial = snapshot(Direction::Buy);
    let created = MeanReversionEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let state = created.next_state;
    let signal_id = state.active_signal.as_ref().unwrap().signal_id;
    let trade_id = Uuid::now_v7();
    let feedback = [PlanFeedback::ActualEntry {
        signal_id,
        virtual_trade_id: trade_id,
        time: 240_000,
        price: Decimal::new(4_002, 0),
    }];
    let mut opposite = snapshot(Direction::Sell);
    push_market_close(&mut opposite, 240_000, Decimal::new(4_003, 0));
    let ignored = MeanReversionEvaluator
        .evaluate(&profile, &opposite, &state, &feedback)
        .unwrap();
    assert_eq!(ignored.next_state.trade_state, TradeState::Open);
    assert!(ignored.directives.is_empty());

    let mut invalid = opposite;
    invalid
        .target
        .facts
        .iter_mut()
        .find(|fact| fact.id.kind == FactKind::Adx)
        .unwrap()
        .value["value"] = json!("30");
    let thesis = ignored
        .next_state
        .active_signal
        .as_ref()
        .unwrap()
        .execution_plan
        .as_ref()
        .unwrap()
        .thesis_invalidation;
    push_market_close(&mut invalid, 300_000, thesis - Decimal::ONE);
    let exited = MeanReversionEvaluator
        .evaluate(&profile, &invalid, &ignored.next_state, &[])
        .unwrap();
    assert_eq!(exited.directives[0].kind, DirectiveKind::EmergencyExit);
    assert_eq!(exited.directives[0].reason, "MEAN_REGIME_INVALIDATED");
}

#[test]
fn runner_health_mean_thesis_close_emits_one_emergency_exit() {
    let profile = profile();
    let initial = snapshot(Direction::Buy);
    let created = MeanReversionEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let signal = created.next_state.active_signal.as_ref().unwrap();
    let signal_id = signal.signal_id;
    let thesis = signal.execution_plan.as_ref().unwrap().thesis_invalidation;
    let trade_id = Uuid::now_v7();
    let mut invalid = initial;
    push_market_close(&mut invalid, 240_000, thesis - Decimal::ONE);
    let feedback = [
        PlanFeedback::ActualEntry {
            signal_id,
            virtual_trade_id: trade_id,
            time: 240_000,
            price: Decimal::new(4_002, 0),
        },
        PlanFeedback::PartialTargetFilled {
            signal_id,
            virtual_trade_id: trade_id,
            time: 260_000,
            price: Decimal::new(4_046, 0),
            realized_fraction: Decimal::new(5, 1),
            remaining_fraction: Decimal::new(5, 1),
        },
    ];

    let managed = MeanReversionEvaluator
        .evaluate(&profile, &invalid, &created.next_state, &feedback)
        .unwrap();

    assert_eq!(managed.directives.len(), 1);
    assert_eq!(managed.directives[0].kind, DirectiveKind::EmergencyExit);
    assert_eq!(managed.directives[0].reason, "MEAN_THESIS_INVALIDATED");
}

#[test]
fn runner_health_partial_fill_owns_breakeven_without_redundant_move_stop() {
    let profile = profile();
    let initial = snapshot(Direction::Buy);
    let created = MeanReversionEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let state = created.next_state;
    let signal_id = state.active_signal.as_ref().unwrap().signal_id;
    let trade_id = Uuid::now_v7();
    let mut favorable = initial;
    push_market_close(&mut favorable, 240_000, Decimal::new(4_046, 0));

    let evaluation = MeanReversionEvaluator
        .evaluate(
            &profile,
            &favorable,
            &state,
            &[
                PlanFeedback::ActualEntry {
                    signal_id,
                    virtual_trade_id: trade_id,
                    time: 240_000,
                    price: Decimal::new(4_002, 0),
                },
                PlanFeedback::PartialTargetFilled {
                    signal_id,
                    virtual_trade_id: trade_id,
                    time: 240_001,
                    price: Decimal::new(4_046, 0),
                    realized_fraction: Decimal::new(5, 1),
                    remaining_fraction: Decimal::new(5, 1),
                },
            ],
        )
        .unwrap();

    assert!(evaluation.directives.is_empty());
    assert_eq!(
        evaluation.next_state.position_stage,
        analysis_plans::PositionStage::Runner
    );
}

#[test]
fn closed_trade_cannot_reuse_consumed_extreme_sequence() {
    let profile = profile();
    let snapshot = snapshot(Direction::Buy);
    let created = MeanReversionEvaluator
        .evaluate(
            &profile,
            &snapshot,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let state = created.next_state;
    let signal_id = state.active_signal.as_ref().unwrap().signal_id;
    let trade_id = Uuid::now_v7();
    let closed = MeanReversionEvaluator
        .evaluate(
            &profile,
            &snapshot,
            &state,
            &[
                PlanFeedback::ActualEntry {
                    signal_id,
                    virtual_trade_id: trade_id,
                    time: 240_000,
                    price: Decimal::new(4_002, 0),
                },
                PlanFeedback::TradeClosed {
                    signal_id,
                    virtual_trade_id: trade_id,
                    time: 300_000,
                    reason: "TARGET".to_owned(),
                },
            ],
        )
        .unwrap();
    assert!(closed.next_state.active_signal.is_none());
    assert!(
        MeanReversionEvaluator
            .evaluate(&profile, &snapshot, &closed.next_state, &[])
            .unwrap()
            .signal
            .is_none()
    );

    let mut fresh = snapshot;
    fresh.target.facts.extend([
        band(
            10,
            300_000,
            Decimal::new(3_980, 0),
            Decimal::new(3_970, 0),
            Decimal::new(3_990, 0),
            Decimal::new(-3, 0),
        ),
        band(
            11,
            360_000,
            Decimal::new(3_975, 0),
            Decimal::new(3_960, 0),
            Decimal::new(3_985, 0),
            Decimal::new(-35, 1),
        ),
        band(
            12,
            420_000,
            Decimal::new(4_001, 0),
            Decimal::new(3_990, 0),
            Decimal::new(4_010, 0),
            Decimal::new(-5, 1),
        ),
    ]);
    fresh.evaluated_closed_candle_time = 479_999;
    assert!(
        MeanReversionEvaluator
            .evaluate(&profile, &fresh, &closed.next_state, &[])
            .unwrap()
            .signal
            .is_some()
    );
}
