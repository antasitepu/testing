use analysis_core::{
    ConfirmedSwing, DivergenceKind, RegularDivergence, StructureEvent, StructureEventKind,
    SwingKind,
};
use analysis_domain::MaterializedProfile;
use analysis_domain::{
    CanonicalMarketSnapshot, ConfirmedFact, Direction, DirectiveKind, ExecutionInstruction, FactId,
    FactKind, FactLifecycle, HtfAlignment, HtfContext, PlanFeedback, PlanKind, PlanParameters,
    PlanProfileKey, PrimitiveVersion, Readiness, ReadinessReason, RsiPlanParameters, SetupState,
    SignalDefinition, TimeframeFacts, TradeState, VersionIdentity,
};
use analysis_plans::{
    PlanError, PlanEvaluator, PlanRuntimeState, PositionStage, RsiEvaluator, RsiV5Evaluator,
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

fn htf_id(kind: FactKind, ordinal: u32, confirmed_at: i64) -> FactId {
    FactId {
        symbol: symbol(),
        timeframe: Timeframe::M5,
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

fn profile(divergence_required: bool) -> MaterializedProfile {
    MaterializedProfile {
        key: PlanProfileKey {
            symbol: symbol(),
            plan: PlanKind::Rsi,
            target_timeframe: Timeframe::M1,
        },
        enabled: true,
        profit_protection: analysis_domain::ProfitProtectionParameters::default(),
        position_management: analysis_domain::PositionManagementParameters::default(),
        versions: VersionIdentity::new(Uuid::now_v7(), "rsi-v1", "rsi-1m-v1").unwrap(),
        parameters: PlanParameters::Rsi(RsiPlanParameters {
            rsi_period: 14,
            oversold_threshold: Decimal::new(30, 0),
            overbought_threshold: Decimal::new(70, 0),
            divergence_required,
            minimum_pivot_distance: 2,
            maximum_pivot_distance: 20,
            confirmed_swing_lookback: 10,
            atr_period: 14,
            atr_buffer_multiplier: Decimal::new(2, 1),
            target_rr: Decimal::TWO,
            max_bars_valid: 5,
            reversal_confirmation_bars: 2,
        }),
        required_timeframes: Vec::new(),
        minimum_history_bars: 100,
        warmup_bars: 20,
    }
}

fn v5_profile(divergence_required: bool, reversal_confirmation_bars: i32) -> MaterializedProfile {
    let mut profile = profile(divergence_required);
    profile.versions.algorithm_version = "rsi-v5".to_owned();
    let PlanParameters::Rsi(parameters) = &mut profile.parameters else {
        unreachable!("RSI profile helper must contain RSI parameters");
    };
    parameters.reversal_confirmation_bars = reversal_confirmation_bars;
    profile
}

fn snapshot(direction: Direction, divergence: bool) -> CanonicalMarketSnapshot {
    let (extreme, exit, close, swing_kind, swing_price, divergence_kind) = match direction {
        Direction::Buy => (
            Decimal::new(25, 0),
            Decimal::new(35, 0),
            Decimal::new(4_100, 0),
            SwingKind::Low,
            Decimal::new(4_080, 0),
            DivergenceKind::RegularBullish,
        ),
        Direction::Sell => (
            Decimal::new(75, 0),
            Decimal::new(65, 0),
            Decimal::new(4_100, 0),
            SwingKind::High,
            Decimal::new(4_120, 0),
            DivergenceKind::RegularBearish,
        ),
        Direction::None => unreachable!(),
    };
    let swing_kind_id = if swing_kind == SwingKind::Low {
        FactKind::SwingLow
    } else {
        FactKind::SwingHigh
    };
    let swing_id = id(swing_kind_id, 3, 150_000);
    let first_rsi_id = id(FactKind::Rsi, 1, 120_000);
    let exit_rsi_id = id(FactKind::Rsi, 2, 180_000);
    let atr_id = id(FactKind::Atr, 4, 180_000);
    let close_id = id(FactKind::MarketClose, 6, 180_000);
    let mut facts = vec![
        fact(
            first_rsi_id.clone(),
            json!({"value": extreme.to_string(), "close": "4090"}),
        ),
        fact(
            exit_rsi_id.clone(),
            json!({"value": exit.to_string(), "close": close.to_string()}),
        ),
        fact(
            swing_id.clone(),
            ConfirmedSwing {
                id: swing_id.clone(),
                kind: swing_kind,
                label: None,
                price: swing_price,
                pivot_open_time: 120_000,
                confirmed_at: 150_000,
                source_start_time: 60_000,
                source_end_time: 150_000,
            },
        ),
        fact(atr_id, json!({"value": "10"})),
        fact(close_id, json!({"value": close.to_string()})),
        // Malformed unrelated facts prove this standalone evaluator never deserializes them.
        fact(id(FactKind::Liquidity, 90, 180_000), json!({"bad": true})),
        fact(id(FactKind::FairValueGap, 91, 180_000), json!("bad")),
        fact(id(FactKind::Structure, 92, 180_000), json!(null)),
    ];
    if divergence {
        let divergence_id = id(FactKind::RsiDivergence, 5, 180_000);
        facts.push(fact(
            divergence_id.clone(),
            RegularDivergence {
                id: divergence_id,
                kind: divergence_kind,
                first_price_swing_id: id(swing_kind_id, 0, 60_000),
                second_price_swing_id: swing_id,
                first_rsi_pivot_id: id(FactKind::Rsi, 80, 60_000),
                second_rsi_pivot_id: exit_rsi_id,
            },
        ));
    }
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

fn not_ready_snapshot() -> CanonicalMarketSnapshot {
    let mut snapshot = snapshot(Direction::Buy, false);
    snapshot.readiness = Readiness::NotReady {
        reason: ReadinessReason::InsufficientHistory,
    };
    snapshot
}

fn v5_rsi_sequence_snapshot(
    direction: Direction,
    observations: &[(i64, Decimal, Decimal)],
) -> CanonicalMarketSnapshot {
    let mut snapshot = snapshot(direction, false);
    snapshot
        .target
        .facts
        .retain(|fact| fact.id.kind != FactKind::Rsi);
    for (index, (confirmed_at, rsi, close)) in observations.iter().enumerate() {
        snapshot.target.facts.push(fact(
            id(FactKind::Rsi, 100 + index as u32, *confirmed_at),
            json!({"value": rsi.to_string(), "close": close.to_string()}),
        ));
    }
    let latest = observations
        .last()
        .expect("v5 RSI sequence fixture requires an observation")
        .0;
    snapshot.evaluated_closed_candle_time = latest + 59_999;
    snapshot.target.evaluated_closed_candle_time = snapshot.evaluated_closed_candle_time;
    snapshot
}

fn set_v5_supporting_swing_price(
    snapshot: &mut CanonicalMarketSnapshot,
    direction: Direction,
    price: Decimal,
) {
    let swing_kind = match direction {
        Direction::Buy => FactKind::SwingLow,
        Direction::Sell => FactKind::SwingHigh,
        Direction::None => unreachable!(),
    };
    let swing_fact = snapshot
        .target
        .facts
        .iter_mut()
        .find(|fact| fact.id.kind == swing_kind)
        .expect("supporting swing fact");
    let mut swing: ConfirmedSwing =
        serde_json::from_value(swing_fact.value.clone()).expect("supporting swing payload");
    swing.price = price;
    swing_fact.value = serde_json::to_value(swing).expect("updated supporting swing payload");
}

fn v5_extreme_snapshot(direction: Direction) -> CanonicalMarketSnapshot {
    let extreme = match direction {
        Direction::Buy => Decimal::new(25, 0),
        Direction::Sell => Decimal::new(75, 0),
        Direction::None => unreachable!(),
    };
    v5_rsi_sequence_snapshot(direction, &[(120_000, extreme, Decimal::new(4_090, 0))])
}

fn v5_confirmation_snapshot(
    direction: Direction,
    confirming_rsi: Decimal,
    confirming_close: Decimal,
    confirmed_at: i64,
) -> CanonicalMarketSnapshot {
    let extreme = match direction {
        Direction::Buy => Decimal::new(25, 0),
        Direction::Sell => Decimal::new(75, 0),
        Direction::None => unreachable!(),
    };
    v5_rsi_sequence_snapshot(
        direction,
        &[
            (120_000, extreme, Decimal::new(4_090, 0)),
            (confirmed_at, confirming_rsi, confirming_close),
        ],
    )
}

fn push_v5_divergence(snapshot: &mut CanonicalMarketSnapshot, direction: Direction) {
    let (swing_kind, divergence_kind) = match direction {
        Direction::Buy => (FactKind::SwingLow, DivergenceKind::RegularBullish),
        Direction::Sell => (FactKind::SwingHigh, DivergenceKind::RegularBearish),
        Direction::None => unreachable!(),
    };
    let divergence_id = id(FactKind::RsiDivergence, 105, 180_000);
    snapshot.target.facts.push(fact(
        divergence_id.clone(),
        RegularDivergence {
            id: divergence_id,
            kind: divergence_kind,
            first_price_swing_id: id(swing_kind, 0, 60_000),
            second_price_swing_id: id(swing_kind, 3, 150_000),
            first_rsi_pivot_id: id(FactKind::Rsi, 80, 60_000),
            second_rsi_pivot_id: id(FactKind::Rsi, 101, 180_000),
        },
    ));
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

fn runner_feedback(
    signal_id: Uuid,
    virtual_trade_id: Uuid,
    time: i64,
    entry: Decimal,
) -> [PlanFeedback; 2] {
    [
        PlanFeedback::ActualEntry {
            signal_id,
            virtual_trade_id,
            time,
            price: entry,
        },
        PlanFeedback::PartialTargetFilled {
            signal_id,
            virtual_trade_id,
            time: time + 1,
            price: entry,
            realized_fraction: Decimal::new(5, 1),
            remaining_fraction: Decimal::new(5, 1),
        },
    ]
}

fn push_rsi_and_close(
    snapshot: &mut CanonicalMarketSnapshot,
    confirmed_at: i64,
    rsi: Decimal,
    close: Decimal,
) {
    snapshot.target.facts.push(fact(
        id(FactKind::Rsi, 201, confirmed_at),
        json!({"value": rsi.to_string(), "close": close.to_string()}),
    ));
    push_market_close(snapshot, confirmed_at, close);
}

fn with_htf_structure(
    mut snapshot: CanonicalMarketSnapshot,
    htf_direction: Direction,
) -> CanonicalMarketSnapshot {
    let structure_id = htf_id(FactKind::Structure, 1, 180_000);
    let broken_swing_id = htf_id(FactKind::SwingHigh, 1, 120_000);
    snapshot.higher_timeframes = vec![TimeframeFacts {
        timeframe: Timeframe::M5,
        evaluated_closed_candle_time: snapshot.evaluated_closed_candle_time,
        facts: vec![fact(
            structure_id.clone(),
            StructureEvent {
                id: structure_id,
                kind: StructureEventKind::BreakOfStructure,
                direction: htf_direction,
                broken_swing_id,
                confirmation_candle_open_time: 180_000,
                confirmation_close: Decimal::new(4_105, 0),
            },
        )],
    }];
    snapshot
}

#[test]
fn evaluator_versions_keep_runtime_v4_separate_from_research_v5() {
    assert_eq!(RsiEvaluator.algorithm_version(), "rsi-v4");
    assert_eq!(RsiV5Evaluator.algorithm_version(), "rsi-v5");
}

#[test]
fn v5_state_decoder_preserves_legacy_shared_trade_state() {
    let profile = v5_profile(false, 2);
    let mut state = PlanRuntimeState::neutral(&profile);
    state.internal = serde_json::json!({
        "entryPrice": "4100",
        "initialRisk": "10",
        "activeStop": "4090"
    });
    let evaluation = RsiV5Evaluator
        .evaluate(&profile, &not_ready_snapshot(), &state, &[])
        .unwrap();
    assert_eq!(
        evaluation.next_state.internal["sharedTrade"]["entryPrice"],
        serde_json::json!("4100")
    );
}

#[test]
fn malformed_v5_state_is_not_silently_reset() {
    let profile = v5_profile(false, 2);
    let mut state = PlanRuntimeState::neutral(&profile);
    state.internal = serde_json::json!({"sharedTrade": "invalid"});
    assert!(matches!(
        RsiV5Evaluator.evaluate(&profile, &not_ready_snapshot(), &state, &[]),
        Err(PlanError::Serialization(_))
    ));
}

#[test]
fn v5_state_decoder_rejects_unknown_and_mixed_typed_legacy_shapes() {
    let profile = v5_profile(false, 2);
    for internal in [
        json!({"unexpectedCorruptField": true}),
        json!({"armed": null, "sharedTrade": {}, "entryPrice": "4100"}),
        json!({
            "armed": null,
            "consumedThrough": null,
            "sharedTrade": {},
            "unexpectedCorruptField": true
        }),
        json!({
            "armed": null,
            "consumedThrough": null,
            "sharedTrade": {"unexpectedCorruptField": true}
        }),
    ] {
        let mut state = PlanRuntimeState::neutral(&profile);
        state.internal = internal;

        assert!(matches!(
            RsiV5Evaluator.evaluate(&profile, &not_ready_snapshot(), &state, &[]),
            Err(PlanError::Serialization(_))
        ));
    }
}

#[test]
fn v5_state_decoder_rejects_nested_fact_id_and_semantic_identity_corruption() {
    let profile = v5_profile(false, 2);
    let without_divergence = RsiV5Evaluator
        .evaluate(
            &profile,
            &v5_extreme_snapshot(Direction::Buy),
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap()
        .next_state
        .internal;
    let mut divergence_snapshot = v5_rsi_sequence_snapshot(
        Direction::Buy,
        &[(180_000, Decimal::new(25, 0), Decimal::new(4_090, 0))],
    );
    push_v5_divergence(&mut divergence_snapshot, Direction::Buy);
    let with_divergence = RsiV5Evaluator
        .evaluate(
            &profile,
            &divergence_snapshot,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap()
        .next_state
        .internal;
    assert!(with_divergence["armed"]["divergenceId"].is_object());

    let mut nested_extreme_unknown = without_divergence.clone();
    nested_extreme_unknown["armed"]["extremeId"]
        .as_object_mut()
        .unwrap()
        .insert("unexpectedCorruptField".to_owned(), json!(true));
    let mut nested_swing_unknown = without_divergence.clone();
    nested_swing_unknown["armed"]["swingId"]
        .as_object_mut()
        .unwrap()
        .insert("unexpectedCorruptField".to_owned(), json!(true));
    let mut nested_divergence_unknown = with_divergence.clone();
    nested_divergence_unknown["armed"]["divergenceId"]
        .as_object_mut()
        .unwrap()
        .insert("unexpectedCorruptField".to_owned(), json!(true));

    let mut extreme_mismatch = without_divergence.clone();
    extreme_mismatch["armed"]["extremeIdentity"]["confirmedAt"] = json!(60_000);
    let mut swing_mismatch = without_divergence.clone();
    swing_mismatch["armed"]["swingIdentity"]["confirmedAt"] = json!(60_000);

    let mut matching_divergence_identity = with_divergence["armed"]["divergenceId"].clone();
    matching_divergence_identity
        .as_object_mut()
        .unwrap()
        .remove("ordinal");
    let mut divergence_mismatch = with_divergence.clone();
    divergence_mismatch["armed"]["divergenceIdentity"] = matching_divergence_identity.clone();
    divergence_mismatch["armed"]["divergenceIdentity"]["confirmedAt"] = json!(60_000);

    let mut divergence_id_without_identity = with_divergence.clone();
    divergence_id_without_identity["armed"]["divergenceIdentity"] = serde_json::Value::Null;
    let mut divergence_identity_without_id = without_divergence.clone();
    divergence_identity_without_id["armed"]["divergenceIdentity"] = matching_divergence_identity;

    let mut null_extreme_identity = without_divergence.clone();
    null_extreme_identity["armed"]["extremeIdentity"] = serde_json::Value::Null;
    let mut null_swing_identity = without_divergence.clone();
    null_swing_identity["armed"]["swingIdentity"] = serde_json::Value::Null;

    for (internal, expected) in [
        (
            nested_extreme_unknown,
            "RSI persisted extremeId contains unknown field(s): unexpectedCorruptField",
        ),
        (
            nested_swing_unknown,
            "RSI persisted swingId contains unknown field(s): unexpectedCorruptField",
        ),
        (
            nested_divergence_unknown,
            "RSI persisted divergenceId contains unknown field(s): unexpectedCorruptField",
        ),
        (
            extreme_mismatch,
            "RSI persisted extreme identity does not match extremeId",
        ),
        (
            swing_mismatch,
            "RSI persisted swing identity does not match swingId",
        ),
        (
            divergence_mismatch,
            "RSI persisted divergence identity does not match divergenceId",
        ),
        (
            divergence_id_without_identity,
            "RSI persisted divergenceId and divergenceIdentity must both be present or both be null",
        ),
        (
            divergence_identity_without_id,
            "RSI persisted divergenceId and divergenceIdentity must both be present or both be null",
        ),
        (
            null_extreme_identity,
            "RSI persisted extremeIdentity cannot be null",
        ),
        (
            null_swing_identity,
            "RSI persisted swingIdentity cannot be null",
        ),
    ] {
        let mut state = PlanRuntimeState::neutral(&profile);
        state.internal = internal;

        assert_eq!(
            RsiV5Evaluator
                .evaluate(&profile, &not_ready_snapshot(), &state, &[])
                .unwrap_err(),
            PlanError::Serialization(expected.to_owned())
        );
    }
}

#[test]
fn v5_current_and_pre_identity_divergence_states_remain_compatible() {
    let profile = v5_profile(false, 2);
    let mut snapshot = v5_rsi_sequence_snapshot(
        Direction::Buy,
        &[(180_000, Decimal::new(25, 0), Decimal::new(4_090, 0))],
    );
    push_v5_divergence(&mut snapshot, Direction::Buy);
    let current = RsiV5Evaluator
        .evaluate(
            &profile,
            &snapshot,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let mut expected_identity = current.next_state.internal["armed"]["divergenceId"].clone();
    expected_identity.as_object_mut().unwrap().remove("ordinal");
    assert_eq!(
        current.next_state.internal["armed"]["divergenceIdentity"],
        expected_identity
    );

    let mut pre_identity = current.next_state;
    pre_identity.internal["armed"]
        .as_object_mut()
        .unwrap()
        .remove("divergenceIdentity");
    let migrated = RsiV5Evaluator
        .evaluate(&profile, &snapshot, &pre_identity, &[])
        .unwrap();
    assert_eq!(
        migrated.next_state.internal["armed"]["divergenceIdentity"],
        expected_identity
    );

    let mut explicit_none = RsiV5Evaluator
        .evaluate(
            &profile,
            &v5_extreme_snapshot(Direction::Buy),
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap()
        .next_state;
    explicit_none.internal["armed"]["divergenceIdentity"] = serde_json::Value::Null;
    assert!(
        RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_extreme_snapshot(Direction::Buy),
                &explicit_none,
                &[]
            )
            .is_ok()
    );
}

#[test]
fn v5_extreme_arms_without_signal() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(false, 2);
        let evaluation = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_extreme_snapshot(direction),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();

        assert!(evaluation.signal.is_none());
        assert_eq!(evaluation.next_state.setup_state, SetupState::Watching);
        assert_eq!(
            evaluation.next_state.internal["armed"]["direction"],
            json!(direction)
        );
    }
}

#[test]
fn v5_persisted_evidence_survives_window_local_ordinal_drift() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(false, 2);
        let armed = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_extreme_snapshot(direction),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();

        assert_eq!(
            armed.next_state.internal["armed"]["extremeIdentity"]["confirmedAt"],
            json!(120_000)
        );
        assert_eq!(
            armed.next_state.internal["armed"]["swingIdentity"]["confirmedAt"],
            json!(150_000)
        );
        assert!(
            armed.next_state.internal["armed"]["extremeIdentity"]
                .get("ordinal")
                .is_none()
        );
        assert!(
            armed.next_state.internal["armed"]["swingIdentity"]
                .get("ordinal")
                .is_none()
        );

        // Remove the new identity fields to exercise migration from an already persisted
        // typed RSI v5 state written by the pre-fix representation.
        let mut legacy_typed_state = armed.next_state;
        let armed_object = legacy_typed_state.internal["armed"]
            .as_object_mut()
            .expect("armed state");
        armed_object.remove("extremeIdentity");
        armed_object.remove("swingIdentity");

        let (confirming_rsi, confirming_close, swing_kind) = match direction {
            Direction::Buy => (
                Decimal::new(27, 0),
                Decimal::new(4_091, 0),
                FactKind::SwingLow,
            ),
            Direction::Sell => (
                Decimal::new(73, 0),
                Decimal::new(4_089, 0),
                FactKind::SwingHigh,
            ),
            Direction::None => unreachable!(),
        };
        let mut shifted =
            v5_confirmation_snapshot(direction, confirming_rsi, confirming_close, 180_000);
        for fact in &mut shifted.target.facts {
            if fact.id.kind == FactKind::Rsi && fact.id.confirmed_at == 120_000 {
                fact.id.ordinal = 17;
            }
            if fact.id.kind == swing_kind {
                fact.id.ordinal = 2;
                let mut swing: ConfirmedSwing =
                    serde_json::from_value(fact.value.clone()).expect("swing payload");
                swing.id = fact.id.clone();
                fact.value = serde_json::to_value(swing).expect("shifted swing payload");
            }
        }

        let confirmed = RsiV5Evaluator
            .evaluate(&profile, &shifted, &legacy_typed_state, &[])
            .unwrap();
        let signal = confirmed
            .signal
            .expect("semantic evidence must survive window-local ordinal drift");

        assert_eq!(signal.direction, direction);
        assert!(signal.evidence.iter().any(|evidence| {
            evidence.reason == "RSI_EXTREME_ENTERED" && evidence.fact_id.ordinal == 17
        }));
        assert!(signal.evidence.iter().any(|evidence| {
            evidence.reason == "CONFIRMED_SWING" && evidence.fact_id.ordinal == 2
        }));
    }
}

#[test]
fn v5_confirmation_requires_latest_rsi_and_price_reversal_inside_window() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(false, 2);
        let (reversed_rsi, unreversed_rsi, reversed_close, unreversed_close) = match direction {
            Direction::Buy => (
                Decimal::new(27, 0),
                Decimal::new(24, 0),
                Decimal::new(4_091, 0),
                Decimal::new(4_089, 0),
            ),
            Direction::Sell => (
                Decimal::new(73, 0),
                Decimal::new(76, 0),
                Decimal::new(4_089, 0),
                Decimal::new(4_091, 0),
            ),
            Direction::None => unreachable!(),
        };
        for input in [
            v5_confirmation_snapshot(direction, reversed_rsi, unreversed_close, 180_000),
            v5_confirmation_snapshot(direction, unreversed_rsi, reversed_close, 180_000),
        ] {
            let evaluation = RsiV5Evaluator
                .evaluate(&profile, &input, &PlanRuntimeState::neutral(&profile), &[])
                .unwrap();
            assert!(evaluation.signal.is_none());
        }

        let confirmed = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_confirmation_snapshot(direction, reversed_rsi, reversed_close, 180_000),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        let signal = confirmed
            .signal
            .expect("both reversal dimensions should confirm RSI v5");
        assert_eq!(signal.direction, direction);
        assert_eq!(signal.execution_instruction, ExecutionInstruction::NextOpen);

        let expired = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_confirmation_snapshot(direction, reversed_rsi, reversed_close, 300_000),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        assert!(expired.signal.is_none());
        assert!(expired.next_state.internal["armed"].is_null());
        assert_eq!(
            expired.next_state.internal["consumedThrough"],
            json!(240_000),
            "expiry invalidation consumes the entire original confirmation window"
        );
    }
}

#[test]
fn v5_required_divergence_can_confirm_with_the_latest_rsi_fact() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(true, 2);
        let (confirming_rsi, confirming_close) = match direction {
            Direction::Buy => (Decimal::new(27, 0), Decimal::new(4_091, 0)),
            Direction::Sell => (Decimal::new(73, 0), Decimal::new(4_089, 0)),
            Direction::None => unreachable!(),
        };
        let without_divergence =
            v5_confirmation_snapshot(direction, confirming_rsi, confirming_close, 180_000);
        assert!(
            RsiV5Evaluator
                .evaluate(
                    &profile,
                    &without_divergence,
                    &PlanRuntimeState::neutral(&profile),
                    &[],
                )
                .unwrap()
                .signal
                .is_none()
        );

        let mut with_divergence = without_divergence;
        push_v5_divergence(&mut with_divergence, direction);
        let confirmed = RsiV5Evaluator
            .evaluate(
                &profile,
                &with_divergence,
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        let signal = confirmed
            .signal
            .expect("latest divergence evidence should confirm RSI v5");
        assert_eq!(signal.direction, direction);
        assert!(
            signal
                .evidence
                .iter()
                .any(|evidence| evidence.reason == "REGULAR_DIVERGENCE")
        );
    }
}

#[test]
fn v5_reconstruction_signals_only_when_latest_fact_confirms() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(false, 2);
        let (extreme, confirmed_rsi, latest_rsi, confirmed_close, latest_close) = match direction {
            Direction::Buy => (
                Decimal::new(25, 0),
                Decimal::new(27, 0),
                Decimal::new(28, 0),
                Decimal::new(4_091, 0),
                Decimal::new(4_092, 0),
            ),
            Direction::Sell => (
                Decimal::new(75, 0),
                Decimal::new(73, 0),
                Decimal::new(72, 0),
                Decimal::new(4_089, 0),
                Decimal::new(4_088, 0),
            ),
            Direction::None => unreachable!(),
        };
        let reconstructed = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_rsi_sequence_snapshot(
                    direction,
                    &[
                        (120_000, extreme, Decimal::new(4_090, 0)),
                        (180_000, confirmed_rsi, confirmed_close),
                    ],
                ),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        assert_eq!(
            reconstructed
                .signal
                .expect("latest live fact should confirm reconstructed arm")
                .direction,
            direction
        );

        let historical_confirmation = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_rsi_sequence_snapshot(
                    direction,
                    &[
                        (120_000, extreme, Decimal::new(4_090, 0)),
                        (180_000, confirmed_rsi, confirmed_close),
                        (240_000, latest_rsi, latest_close),
                    ],
                ),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        assert!(
            historical_confirmation.signal.is_none(),
            "a historical confirmation must not replay after the latest close"
        );
        assert!(historical_confirmation.next_state.internal["armed"].is_null());
        assert_eq!(
            historical_confirmation.next_state.internal["consumedThrough"],
            json!(180_000)
        );

        let latest_outside_threshold = match direction {
            Direction::Buy => Decimal::new(35, 0),
            Direction::Sell => Decimal::new(65, 0),
            Direction::None => unreachable!(),
        };
        let mut consumed_state = PlanRuntimeState::neutral(&profile);
        consumed_state.internal = json!({
            "armed": null,
            "consumedThrough": 120_000,
            "sharedTrade": {}
        });
        let consumed = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_rsi_sequence_snapshot(
                    direction,
                    &[
                        (120_000, extreme, Decimal::new(4_090, 0)),
                        (180_000, latest_outside_threshold, confirmed_close),
                    ],
                ),
                &consumed_state,
                &[],
            )
            .unwrap();
        assert!(consumed.signal.is_none());
        assert!(consumed.next_state.internal["armed"].is_null());
        assert_eq!(
            consumed.next_state.internal["consumedThrough"],
            json!(120_000)
        );
    }
}

#[test]
fn v5_reconstruction_uses_newest_eligible_same_direction_cycle() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(false, 2);
        let observations = match direction {
            Direction::Buy => [
                (60_000, Decimal::new(20, 0), Decimal::new(4_090, 0)),
                (120_000, Decimal::new(22, 0), Decimal::new(4_091, 0)),
                (180_000, Decimal::new(25, 0), Decimal::new(4_089, 0)),
                (240_000, Decimal::new(26, 0), Decimal::new(4_090, 0)),
            ],
            Direction::Sell => [
                (60_000, Decimal::new(80, 0), Decimal::new(4_090, 0)),
                (120_000, Decimal::new(78, 0), Decimal::new(4_089, 0)),
                (180_000, Decimal::new(75, 0), Decimal::new(4_091, 0)),
                (240_000, Decimal::new(74, 0), Decimal::new(4_090, 0)),
            ],
            Direction::None => unreachable!(),
        };

        let evaluation = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_rsi_sequence_snapshot(direction, &observations),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        let signal = evaluation
            .signal
            .expect("the latest eligible same-direction cycle must confirm");

        assert_eq!(signal.direction, direction);
        assert!(signal.evidence.iter().any(|evidence| {
            evidence.reason == "RSI_EXTREME_ENTERED" && evidence.fact_id.confirmed_at == 180_000
        }));
        assert!(signal.evidence.iter().any(|evidence| {
            evidence.reason == "RSI_REVERSAL_CONFIRMED" && evidence.fact_id.confirmed_at == 240_000
        }));
    }
}

#[test]
fn v5_expiry_reconstruction_uses_newest_eligible_same_direction_cycle() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(false, 2);
        let observations = match direction {
            Direction::Buy => [
                (60_000, Decimal::new(20, 0), Decimal::new(100, 0)),
                (240_000, Decimal::new(25, 0), Decimal::new(99, 0)),
                (300_000, Decimal::new(26, 0), Decimal::new(100, 0)),
            ],
            Direction::Sell => [
                (60_000, Decimal::new(80, 0), Decimal::new(100, 0)),
                (240_000, Decimal::new(75, 0), Decimal::new(101, 0)),
                (300_000, Decimal::new(74, 0), Decimal::new(100, 0)),
            ],
            Direction::None => unreachable!(),
        };

        let mut snapshot = v5_rsi_sequence_snapshot(direction, &observations);
        set_v5_supporting_swing_price(
            &mut snapshot,
            direction,
            match direction {
                Direction::Buy => Decimal::new(90, 0),
                Direction::Sell => Decimal::new(110, 0),
                Direction::None => unreachable!(),
            },
        );
        let evaluation = RsiV5Evaluator
            .evaluate(
                &profile,
                &snapshot,
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        let signal = evaluation
            .signal
            .expect("expiry rescan must recover the newest eligible arm");

        assert_eq!(signal.direction, direction);
        assert!(signal.evidence.iter().any(|evidence| {
            evidence.reason == "RSI_EXTREME_ENTERED" && evidence.fact_id.confirmed_at == 240_000
        }));
        assert!(signal.evidence.iter().any(|evidence| {
            evidence.reason == "RSI_REVERSAL_CONFIRMED" && evidence.fact_id.confirmed_at == 300_000
        }));
    }
}

#[test]
fn v5_expiry_rescan_does_not_rearm_same_direction_price_continuation() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(false, 2);
        let observations = match direction {
            Direction::Buy => [
                (60_000, Decimal::new(20, 0), Decimal::new(100, 0)),
                (240_000, Decimal::new(25, 0), Decimal::new(100, 0)),
                (300_000, Decimal::new(26, 0), Decimal::new(101, 0)),
            ],
            Direction::Sell => [
                (60_000, Decimal::new(80, 0), Decimal::new(100, 0)),
                (240_000, Decimal::new(75, 0), Decimal::new(100, 0)),
                (300_000, Decimal::new(74, 0), Decimal::new(99, 0)),
            ],
            Direction::None => unreachable!(),
        };

        let mut snapshot = v5_rsi_sequence_snapshot(direction, &observations);
        set_v5_supporting_swing_price(
            &mut snapshot,
            direction,
            match direction {
                Direction::Buy => Decimal::new(90, 0),
                Direction::Sell => Decimal::new(110, 0),
                Direction::None => unreachable!(),
            },
        );
        let evaluation = RsiV5Evaluator
            .evaluate(
                &profile,
                &snapshot,
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();

        assert!(
            evaluation.signal.is_none(),
            "expiry rescan must not promote a same-direction price continuation"
        );
    }
}

#[test]
fn v5_post_expiry_retry_does_not_replay_historical_confirmation() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(false, 2);
        let (extreme, historical_rsi, latest_rsi, historical_close, latest_close) = match direction
        {
            Direction::Buy => (
                Decimal::new(25, 0),
                Decimal::new(27, 0),
                Decimal::new(28, 0),
                Decimal::new(4_091, 0),
                Decimal::new(4_092, 0),
            ),
            Direction::Sell => (
                Decimal::new(75, 0),
                Decimal::new(73, 0),
                Decimal::new(72, 0),
                Decimal::new(4_089, 0),
                Decimal::new(4_088, 0),
            ),
            Direction::None => unreachable!(),
        };
        let input = v5_rsi_sequence_snapshot(
            direction,
            &[
                (120_000, extreme, Decimal::new(4_090, 0)),
                (180_000, historical_rsi, historical_close),
                (300_000, latest_rsi, latest_close),
            ],
        );
        let first = RsiV5Evaluator
            .evaluate(&profile, &input, &PlanRuntimeState::neutral(&profile), &[])
            .unwrap();
        assert!(first.signal.is_none());

        let retried = RsiV5Evaluator
            .evaluate(&profile, &input, &first.next_state, &[])
            .unwrap();

        assert!(
            retried.signal.is_none(),
            "an expired historical reversal must remain consumed on retry"
        );
        assert_eq!(first.next_state.internal["consumedThrough"], json!(180_000));
    }
}

#[test]
fn v5_late_historical_reversal_uses_original_expiry_invalidation() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(false, 2);
        let (extreme, historical_rsi, latest_rsi, historical_close, latest_close) = match direction
        {
            Direction::Buy => (
                Decimal::new(25, 0),
                Decimal::new(27, 0),
                Decimal::new(28, 0),
                Decimal::new(4_091, 0),
                Decimal::new(4_092, 0),
            ),
            Direction::Sell => (
                Decimal::new(75, 0),
                Decimal::new(73, 0),
                Decimal::new(72, 0),
                Decimal::new(4_089, 0),
                Decimal::new(4_088, 0),
            ),
            Direction::None => unreachable!(),
        };
        let evaluation = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_rsi_sequence_snapshot(
                    direction,
                    &[
                        (120_000, extreme, Decimal::new(4_090, 0)),
                        (300_000, historical_rsi, historical_close),
                        (360_000, latest_rsi, latest_close),
                    ],
                ),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();

        assert!(evaluation.signal.is_none());
        assert!(evaluation.next_state.internal["armed"].is_null());
        assert_eq!(
            evaluation.next_state.internal["consumedThrough"],
            json!(240_000),
            "a late reversal is invalid and the original expiry window remains consumed"
        );
    }
}

#[test]
fn v5_missing_swing_invalidation_precedes_historical_confirmation() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(false, 20);
        let armed = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_extreme_snapshot(direction),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        let (extreme, historical_rsi, latest_rsi, historical_close, latest_close, swing_kind) =
            match direction {
                Direction::Buy => (
                    Decimal::new(25, 0),
                    Decimal::new(27, 0),
                    Decimal::new(28, 0),
                    Decimal::new(4_091, 0),
                    Decimal::new(4_092, 0),
                    FactKind::SwingLow,
                ),
                Direction::Sell => (
                    Decimal::new(75, 0),
                    Decimal::new(73, 0),
                    Decimal::new(72, 0),
                    Decimal::new(4_089, 0),
                    Decimal::new(4_088, 0),
                    FactKind::SwingHigh,
                ),
                Direction::None => unreachable!(),
            };
        let mut missing = v5_rsi_sequence_snapshot(
            direction,
            &[
                (120_000, extreme, Decimal::new(4_090, 0)),
                (180_000, historical_rsi, historical_close),
                (240_000, latest_rsi, latest_close),
            ],
        );
        missing
            .target
            .facts
            .retain(|fact| fact.id.kind != swing_kind);

        let invalidated = RsiV5Evaluator
            .evaluate(&profile, &missing, &armed.next_state, &[])
            .unwrap();

        assert!(invalidated.signal.is_none());
        assert!(invalidated.next_state.internal["armed"].is_null());
        assert_eq!(
            invalidated.next_state.internal["consumedThrough"],
            json!(120_000)
        );
    }
}

#[test]
fn v5_stale_swing_invalidation_precedes_historical_confirmation() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(false, 20);
        let armed = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_extreme_snapshot(direction),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        let (extreme, historical_rsi, latest_rsi, historical_close, latest_close) = match direction
        {
            Direction::Buy => (
                Decimal::new(25, 0),
                Decimal::new(27, 0),
                Decimal::new(28, 0),
                Decimal::new(4_091, 0),
                Decimal::new(4_092, 0),
            ),
            Direction::Sell => (
                Decimal::new(75, 0),
                Decimal::new(73, 0),
                Decimal::new(72, 0),
                Decimal::new(4_089, 0),
                Decimal::new(4_088, 0),
            ),
            Direction::None => unreachable!(),
        };
        let stale = v5_rsi_sequence_snapshot(
            direction,
            &[
                (120_000, extreme, Decimal::new(4_090, 0)),
                (180_000, historical_rsi, historical_close),
                (840_000, latest_rsi, latest_close),
            ],
        );

        let invalidated = RsiV5Evaluator
            .evaluate(&profile, &stale, &armed.next_state, &[])
            .unwrap();

        assert!(invalidated.signal.is_none());
        assert!(invalidated.next_state.internal["armed"].is_null());
        assert_eq!(
            invalidated.next_state.internal["consumedThrough"],
            json!(120_000)
        );
    }
}

#[test]
fn v5_exact_original_expiry_boundary_still_confirms() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(false, 2);
        let (extreme, confirming_rsi, confirming_close) = match direction {
            Direction::Buy => (
                Decimal::new(25, 0),
                Decimal::new(27, 0),
                Decimal::new(4_091, 0),
            ),
            Direction::Sell => (
                Decimal::new(75, 0),
                Decimal::new(73, 0),
                Decimal::new(4_089, 0),
            ),
            Direction::None => unreachable!(),
        };
        let evaluation = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_rsi_sequence_snapshot(
                    direction,
                    &[
                        (120_000, extreme, Decimal::new(4_090, 0)),
                        (240_000, confirming_rsi, confirming_close),
                    ],
                ),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();

        assert_eq!(
            evaluation
                .signal
                .expect("confirmation at the exact expiry boundary remains valid")
                .direction,
            direction
        );
    }
}

#[test]
fn v5_persisted_arm_adopts_optional_divergence_on_confirmation() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(false, 2);
        let armed = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_extreme_snapshot(direction),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        assert!(armed.next_state.internal["armed"]["divergenceId"].is_null());

        let (confirming_rsi, confirming_close) = match direction {
            Direction::Buy => (Decimal::new(27, 0), Decimal::new(4_091, 0)),
            Direction::Sell => (Decimal::new(73, 0), Decimal::new(4_089, 0)),
            Direction::None => unreachable!(),
        };
        let mut confirmation =
            v5_confirmation_snapshot(direction, confirming_rsi, confirming_close, 180_000);
        push_v5_divergence(&mut confirmation, direction);

        let evaluation = RsiV5Evaluator
            .evaluate(&profile, &confirmation, &armed.next_state, &[])
            .unwrap();
        let signal = evaluation
            .signal
            .expect("optional divergence should enrich the confirming signal");

        assert_eq!(signal.direction, direction);
        assert!(signal.evidence.iter().any(|evidence| {
            evidence.reason == "REGULAR_DIVERGENCE"
                && evidence.fact_id.kind == FactKind::RsiDivergence
                && evidence.fact_id.confirmed_at == 180_000
        }));
    }
}

#[test]
fn v5_not_ready_invalidates_existing_arm() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(false, 2);
        let armed = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_extreme_snapshot(direction),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        let mut unavailable = v5_extreme_snapshot(direction);
        unavailable.readiness = Readiness::NotReady {
            reason: ReadinessReason::InsufficientHistory,
        };

        let invalidated = RsiV5Evaluator
            .evaluate(&profile, &unavailable, &armed.next_state, &[])
            .unwrap();

        assert!(invalidated.signal.is_none());
        assert_eq!(invalidated.next_state.setup_state, SetupState::Watching);
        assert_eq!(invalidated.next_state.readiness, unavailable.readiness);
        assert!(invalidated.next_state.internal["armed"].is_null());
        assert_eq!(
            invalidated.next_state.internal["consumedThrough"],
            json!(120_000)
        );
    }
}

#[test]
fn v5_stale_supporting_swing_invalidates_existing_arm() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(false, 20);
        let armed = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_extreme_snapshot(direction),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        let (confirming_rsi, confirming_close) = match direction {
            Direction::Buy => (Decimal::new(27, 0), Decimal::new(4_091, 0)),
            Direction::Sell => (Decimal::new(73, 0), Decimal::new(4_089, 0)),
            Direction::None => unreachable!(),
        };
        let stale = v5_confirmation_snapshot(direction, confirming_rsi, confirming_close, 840_000);

        let invalidated = RsiV5Evaluator
            .evaluate(&profile, &stale, &armed.next_state, &[])
            .unwrap();

        assert!(invalidated.signal.is_none());
        assert_eq!(invalidated.next_state.setup_state, SetupState::Watching);
        assert!(invalidated.next_state.internal["armed"].is_null());
        assert_eq!(
            invalidated.next_state.internal["consumedThrough"],
            json!(120_000)
        );
    }
}

#[test]
fn v5_extremes_replace_arms_symmetrically_without_signal() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = v5_profile(false, 2);
        let (first, weaker, strongest) = match direction {
            Direction::Buy => (
                Decimal::new(25, 0),
                Decimal::new(27, 0),
                Decimal::new(23, 0),
            ),
            Direction::Sell => (
                Decimal::new(75, 0),
                Decimal::new(73, 0),
                Decimal::new(77, 0),
            ),
            Direction::None => unreachable!(),
        };
        let strongest_arm = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_rsi_sequence_snapshot(
                    direction,
                    &[
                        (120_000, first, Decimal::new(4_090, 0)),
                        (180_000, weaker, Decimal::new(4_090, 0)),
                        (240_000, strongest, Decimal::new(4_090, 0)),
                    ],
                ),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        assert!(strongest_arm.signal.is_none());
        assert_eq!(
            strongest_arm.next_state.internal["armed"]["extremeRsi"],
            json!(strongest.to_string())
        );
        assert_eq!(
            strongest_arm.next_state.internal["armed"]["armedAt"],
            json!(240_000)
        );

        let armed = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_extreme_snapshot(direction),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        let opposite = match direction {
            Direction::Buy => Direction::Sell,
            Direction::Sell => Direction::Buy,
            Direction::None => unreachable!(),
        };
        let opposite_extreme = match opposite {
            Direction::Buy => Decimal::new(25, 0),
            Direction::Sell => Decimal::new(75, 0),
            Direction::None => unreachable!(),
        };
        let replaced = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_rsi_sequence_snapshot(
                    opposite,
                    &[
                        (120_000, first, Decimal::new(4_090, 0)),
                        (180_000, opposite_extreme, Decimal::new(4_090, 0)),
                    ],
                ),
                &armed.next_state,
                &[],
            )
            .unwrap();
        assert!(replaced.signal.is_none());
        assert_eq!(
            replaced.next_state.internal["armed"]["direction"],
            json!(opposite)
        );
        assert_eq!(
            replaced.next_state.internal["armed"]["armedAt"],
            json!(180_000)
        );
    }
}

#[test]
fn oversold_and_overbought_exits_emit_symmetric_next_open_signals() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = profile(false);
        let evaluation = RsiEvaluator
            .evaluate(
                &profile,
                &snapshot(direction, false),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        let signal = evaluation.signal.unwrap();
        assert_eq!(signal.direction, direction);
        assert_eq!(signal.execution_instruction, ExecutionInstruction::NextOpen);
        assert_eq!(signal.projected_entry, Some(Decimal::new(4_100, 0)));
        assert_eq!(signal.effective_at, 240_000);
        assert_eq!(signal.risk_reward, Decimal::TWO);
        assert_eq!(
            (signal.primary_target - Decimal::new(4_100, 0)).abs(),
            (Decimal::new(4_100, 0) - signal.stop_loss).abs() * Decimal::TWO
        );
    }
}

#[test]
fn execution_plan_uses_rsi_confirmed_swing_symmetrically() {
    for (direction, thesis) in [
        (Direction::Buy, Decimal::new(4_080, 0)),
        (Direction::Sell, Decimal::new(4_120, 0)),
    ] {
        let profile = profile(false);
        let signal = RsiEvaluator
            .evaluate(
                &profile,
                &snapshot(direction, false),
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
fn signal_captures_farther_confirmed_swing_as_immutable_extension_target() {
    let profile = profile(false);
    let mut input = snapshot(Direction::Buy, false);
    let far = ConfirmedSwing {
        id: id(FactKind::SwingHigh, 80, 180_001),
        kind: SwingKind::High,
        label: None,
        price: Decimal::new(4_250, 0),
        pivot_open_time: 160_000,
        confirmed_at: 180_001,
        source_start_time: 150_000,
        source_end_time: 180_001,
    };
    input.target.facts.push(fact(far.id.clone(), far));

    let signal = RsiEvaluator
        .evaluate(&profile, &input, &PlanRuntimeState::neutral(&profile), &[])
        .unwrap()
        .signal
        .unwrap();

    assert!(signal.reference_levels.iter().any(|level| {
        level.kind == "TP2_EXTENSION_TARGET" && level.price == Decimal::new(4_250, 0)
    }));
}

#[test]
fn execution_plan_uses_minimum_tick_stop_buffer_when_it_is_wider() {
    for (direction, thesis) in [
        (Direction::Buy, Decimal::new(4_080, 0)),
        (Direction::Sell, Decimal::new(4_120, 0)),
    ] {
        let mut profile = profile(false);
        profile.position_management.minimum_buffer_ticks = 50;
        let signal = RsiEvaluator
            .evaluate(
                &profile,
                &snapshot(direction, false),
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
    }
}

#[test]
fn rsi_signal_keeps_frequency_and_marks_counter_htf_context() {
    let profile = profile(false);
    let evaluation = RsiEvaluator
        .evaluate(
            &profile,
            &with_htf_structure(snapshot(Direction::Buy, false), Direction::Sell),
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let signal = evaluation
        .signal
        .expect("RSI signal should still be emitted");
    let context = signal.htf_context.expect("HTF context");

    assert_eq!(context.higher_timeframe, Some(Timeframe::M5));
    assert_eq!(context.direction, Direction::Sell);
    assert_eq!(context.alignment, HtfAlignment::Counter);
    assert_eq!(context.source, "STRUCTURE");
}

#[test]
fn divergence_filter_and_confirmed_swing_lookback_are_enforced() {
    let required = profile(true);
    assert!(
        RsiEvaluator
            .evaluate(
                &required,
                &snapshot(Direction::Buy, false),
                &PlanRuntimeState::neutral(&required),
                &[],
            )
            .unwrap()
            .signal
            .is_none()
    );
    assert!(
        RsiEvaluator
            .evaluate(
                &required,
                &snapshot(Direction::Buy, true),
                &PlanRuntimeState::neutral(&required),
                &[],
            )
            .unwrap()
            .signal
            .is_some()
    );

    let mut stale = snapshot(Direction::Buy, true);
    stale.evaluated_closed_candle_time = 900_000;
    assert!(
        RsiEvaluator
            .evaluate(
                &required,
                &stale,
                &PlanRuntimeState::neutral(&required),
                &[],
            )
            .unwrap()
            .signal
            .is_none()
    );
}

#[test]
fn runner_health_rsi_opposing_threshold_emits_emergency_exit() {
    let profile = profile(false);
    let initial = snapshot(Direction::Buy, false);
    let created = RsiEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let state = created.next_state;
    let signal_id = state.active_signal.as_ref().unwrap().signal_id;
    let virtual_trade_id = Uuid::now_v7();
    let mut opposing = initial;
    let rsi_id = id(FactKind::Rsi, 99, 240_000);
    opposing
        .target
        .facts
        .push(fact(rsi_id, json!({"value": "75", "close": "4200"})));
    opposing.evaluated_closed_candle_time = 299_999;
    let evaluation = RsiEvaluator
        .evaluate(
            &profile,
            &opposing,
            &state,
            &runner_feedback(signal_id, virtual_trade_id, 240_000, Decimal::new(4_105, 0)),
        )
        .unwrap();
    assert_eq!(evaluation.next_state.trade_state, TradeState::Open);
    assert_eq!(evaluation.directives[0].kind, DirectiveKind::EmergencyExit);
    assert_eq!(evaluation.directives[0].effective_at, 300_000);
}

#[test]
fn failed_reversal_exits_buy_and_sell_at_next_open() {
    for (direction, rsi) in [
        (Direction::Buy, Decimal::new(30, 0)),
        (Direction::Sell, Decimal::new(70, 0)),
    ] {
        let profile = profile(false);
        let initial = snapshot(direction, false);
        let created = RsiEvaluator
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
        let close = match direction {
            Direction::Buy => thesis - Decimal::ONE,
            Direction::Sell => thesis + Decimal::ONE,
            Direction::None => unreachable!(),
        };
        let virtual_trade_id = Uuid::now_v7();
        let mut failed = initial;
        push_rsi_and_close(&mut failed, 240_000, rsi, close);

        let evaluation = RsiEvaluator
            .evaluate(
                &profile,
                &failed,
                &created.next_state,
                &runner_feedback(signal_id, virtual_trade_id, 240_000, Decimal::new(4_105, 0)),
            )
            .unwrap();

        assert_eq!(evaluation.directives.len(), 1);
        assert_eq!(evaluation.directives[0].kind, DirectiveKind::EmergencyExit);
        assert_eq!(evaluation.directives[0].reason, "RSI_FAILED_REVERSAL");
        assert_eq!(evaluation.directives[0].effective_at, 300_000);
    }
}

#[test]
fn v5_state_active_management_preserves_typed_shared_trade_wrapper() {
    for (direction, rsi) in [
        (Direction::Buy, Decimal::new(30, 0)),
        (Direction::Sell, Decimal::new(70, 0)),
    ] {
        let profile = v5_profile(false, 2);
        let initial = snapshot(direction, false);
        let created = RsiEvaluator
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
        let close = match direction {
            Direction::Buy => thesis - Decimal::ONE,
            Direction::Sell => thesis + Decimal::ONE,
            Direction::None => unreachable!(),
        };
        let virtual_trade_id = Uuid::now_v7();
        let mut failed = initial;
        push_rsi_and_close(&mut failed, 240_000, rsi, close);
        let real_arm = RsiV5Evaluator
            .evaluate(
                &profile,
                &v5_extreme_snapshot(direction),
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        assert_eq!(
            real_arm.next_state.internal["armed"]["direction"],
            json!(direction)
        );
        let mut typed_state = created.next_state;
        typed_state.internal = real_arm.next_state.internal;

        let evaluation = RsiV5Evaluator
            .evaluate(
                &profile,
                &failed,
                &typed_state,
                &runner_feedback(signal_id, virtual_trade_id, 240_000, Decimal::new(4_105, 0)),
            )
            .unwrap();

        assert_eq!(evaluation.directives.len(), 1);
        assert_eq!(evaluation.directives[0].kind, DirectiveKind::EmergencyExit);
        assert_eq!(evaluation.directives[0].reason, "RSI_FAILED_REVERSAL");
        assert_eq!(evaluation.directives[0].effective_at, 300_000);
        assert!(evaluation.next_state.internal["sharedTrade"].is_object());
        assert_eq!(
            evaluation.next_state.internal["armed"]["direction"],
            json!(direction)
        );
        assert_eq!(
            evaluation.next_state.internal["armed"]["armedAt"],
            json!(120_000)
        );
    }
}

#[test]
fn plan_specific_directive_ids_do_not_collide_when_reasons_differ() {
    let profile = profile(false);
    let initial = snapshot(Direction::Buy, false);
    let created = RsiEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let signal_id = created.next_state.active_signal.as_ref().unwrap().signal_id;
    let trade_id = Uuid::now_v7();
    let feedback = runner_feedback(signal_id, trade_id, 240_000, Decimal::new(4_105, 0));
    let mut failed = initial.clone();
    push_rsi_and_close(
        &mut failed,
        240_000,
        Decimal::new(30, 0),
        Decimal::new(4_070, 0),
    );
    let mut opposing = initial;
    push_rsi_and_close(
        &mut opposing,
        240_000,
        Decimal::new(70, 0),
        Decimal::new(4_110, 0),
    );

    let failed_directive = &RsiEvaluator
        .evaluate(&profile, &failed, &created.next_state, &feedback)
        .unwrap()
        .directives[0];
    let opposing_directive = &RsiEvaluator
        .evaluate(&profile, &opposing, &created.next_state, &feedback)
        .unwrap()
        .directives[0];

    assert_eq!(failed_directive.kind, DirectiveKind::EmergencyExit);
    assert_eq!(opposing_directive.kind, DirectiveKind::EmergencyExit);
    assert_ne!(failed_directive.reason, opposing_directive.reason);
    assert_ne!(
        failed_directive.directive_id,
        opposing_directive.directive_id
    );
}

#[test]
fn runner_health_rsi_thesis_close_is_symmetric() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = profile(false);
        let initial = snapshot(direction, false);
        let created = RsiEvaluator
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
        let invalid_close = match direction {
            Direction::Buy => thesis - Decimal::ONE,
            Direction::Sell => thesis + Decimal::ONE,
            Direction::None => unreachable!(),
        };
        let trade_id = Uuid::now_v7();
        let mut invalid = initial;
        push_market_close(&mut invalid, 240_000, invalid_close);
        let feedback = runner_feedback(signal_id, trade_id, 240_000, Decimal::new(4_105, 0));

        let managed = RsiEvaluator
            .evaluate(&profile, &invalid, &created.next_state, &feedback)
            .unwrap();

        assert_eq!(managed.directives.len(), 1);
        assert_eq!(managed.directives[0].kind, DirectiveKind::EmergencyExit);
        assert_eq!(managed.directives[0].reason, "RSI_THESIS_INVALIDATED");
    }
}

#[test]
fn failed_reversal_requires_adverse_rsi_and_close_together() {
    for (direction, rsi, close) in [
        (Direction::Sell, Decimal::new(70, 0), Decimal::new(4_100, 0)),
        (Direction::Sell, Decimal::new(65, 0), Decimal::new(4_110, 0)),
        (Direction::Buy, Decimal::new(30, 0), Decimal::new(4_110, 0)),
        (Direction::Buy, Decimal::new(35, 0), Decimal::new(4_100, 0)),
    ] {
        let profile = profile(false);
        let initial = snapshot(direction, false);
        let created = RsiEvaluator
            .evaluate(
                &profile,
                &initial,
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        let signal_id = created.next_state.active_signal.as_ref().unwrap().signal_id;
        let mut partial = initial;
        push_rsi_and_close(&mut partial, 240_000, rsi, close);

        let evaluation = RsiEvaluator
            .evaluate(
                &profile,
                &partial,
                &created.next_state,
                &runner_feedback(signal_id, Uuid::now_v7(), 240_000, Decimal::new(4_105, 0)),
            )
            .unwrap();

        assert!(
            evaluation
                .directives
                .iter()
                .all(|directive| directive.reason != "RSI_FAILED_REVERSAL")
        );
    }
}

#[test]
fn runner_health_tp1_feedback_does_not_emit_redundant_breakeven_move() {
    let profile = profile(false);
    let initial = snapshot(Direction::Buy, false);
    let created = RsiEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let state = created.next_state;
    let signal_id = state.active_signal.as_ref().unwrap().signal_id;
    let virtual_trade_id = Uuid::now_v7();
    let mut favorable = initial;
    push_market_close(&mut favorable, 240_000, Decimal::new(4_132, 0));

    let evaluation = RsiEvaluator
        .evaluate(
            &profile,
            &favorable,
            &state,
            &runner_feedback(signal_id, virtual_trade_id, 240_000, Decimal::new(4_105, 0)),
        )
        .unwrap();

    assert!(evaluation.directives.is_empty());
    assert_eq!(
        evaluation.next_state.position_stage,
        analysis_plans::PositionStage::Runner
    );
}

#[test]
fn duplicate_actual_entry_after_tp1_is_an_end_to_end_no_op() {
    let profile = profile(false);
    let initial = snapshot(Direction::Buy, false);
    let created = RsiEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let signal_id = created.next_state.active_signal.as_ref().unwrap().signal_id;
    let virtual_trade_id = Uuid::now_v7();
    let entry = Decimal::new(4_105, 0);
    let once = runner_feedback(signal_id, virtual_trade_id, 240_000, entry);
    let duplicate = [once[0].clone(), once[1].clone(), once[0].clone()];

    let expected = RsiEvaluator
        .evaluate(&profile, &initial, &created.next_state, &once)
        .unwrap();
    let actual = RsiEvaluator
        .evaluate(&profile, &initial, &created.next_state, &duplicate)
        .unwrap();

    assert_eq!(actual, expected);
    assert_eq!(actual.next_state.position_stage, PositionStage::Runner);
    assert_eq!(actual.next_state.remaining_fraction, Decimal::new(5, 1));
    assert_eq!(actual.next_state.realized_r, Decimal::new(5, 1));
}

#[test]
fn runner_health_counter_htf_giveback_weakens_and_tightens_runner_stop() {
    let profile = profile(false);
    let initial = with_htf_structure(snapshot(Direction::Buy, false), Direction::Sell);
    let created = RsiEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let state = created.next_state;
    let signal_id = state.active_signal.as_ref().unwrap().signal_id;
    let virtual_trade_id = Uuid::now_v7();
    let mut favorable = initial;
    push_market_close(&mut favorable, 240_000, Decimal::new(4_120, 0));

    let evaluation = RsiEvaluator
        .evaluate(
            &profile,
            &favorable,
            &state,
            &runner_feedback(signal_id, virtual_trade_id, 240_000, Decimal::new(4_105, 0)),
        )
        .unwrap();

    assert_eq!(evaluation.directives.len(), 1);
    assert_eq!(evaluation.directives[0].kind, DirectiveKind::MoveStop);
    assert!(evaluation.directives[0].price > Some(Decimal::new(4_105, 0)));
    assert_eq!(
        evaluation.next_state.trade_health,
        analysis_plans::TradeHealth::Weakening
    );
}

#[test]
fn stepped_profit_lock_moves_beyond_breakeven_symmetrically() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = profile(false);
        let initial = snapshot(direction, false);
        let created = RsiEvaluator
            .evaluate(
                &profile,
                &initial,
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        let signal = created.next_state.active_signal.as_ref().unwrap().clone();
        let entry = Decimal::new(4_105, 0);
        let risk = (entry - signal.stop_loss).abs();
        let price_at_r = |r: Decimal| match direction {
            Direction::Buy => entry + risk * r,
            Direction::Sell => entry - risk * r,
            Direction::None => unreachable!(),
        };
        let signal_id = signal.signal_id;
        let virtual_trade_id = Uuid::now_v7();

        let mut at_one_r = initial.clone();
        push_market_close(&mut at_one_r, 240_000, price_at_r(Decimal::ONE));
        let breakeven = RsiEvaluator
            .evaluate(
                &profile,
                &at_one_r,
                &created.next_state,
                &runner_feedback(signal_id, virtual_trade_id, 240_000, entry),
            )
            .unwrap();
        assert!(breakeven.directives.is_empty());

        let mut at_one_and_half_r = initial;
        push_market_close(
            &mut at_one_and_half_r,
            300_000,
            price_at_r(Decimal::new(15, 1)),
        );
        let locked = RsiEvaluator
            .evaluate(&profile, &at_one_and_half_r, &breakeven.next_state, &[])
            .unwrap();
        let expected = price_at_r(Decimal::new(5, 1));

        assert_eq!(locked.directives[0].kind, DirectiveKind::MoveStop);
        assert_eq!(locked.directives[0].price, Some(expected));
        assert_eq!(locked.directives[0].reason, "RSI_STEPPED_PROFIT_LOCK");
    }
}

#[test]
fn profitable_two_close_reversal_suggests_close_symmetrically() {
    for direction in [Direction::Buy, Direction::Sell] {
        let profile = profile(false);
        let initial = snapshot(direction, false);
        let created = RsiEvaluator
            .evaluate(
                &profile,
                &initial,
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        let signal = created.next_state.active_signal.as_ref().unwrap().clone();
        let entry = Decimal::new(4_105, 0);
        let risk = (entry - signal.stop_loss).abs();
        let price_at_r = |r: Decimal| match direction {
            Direction::Buy => entry + risk * r,
            Direction::Sell => entry - risk * r,
            Direction::None => unreachable!(),
        };
        let signal_id = signal.signal_id;
        let virtual_trade_id = Uuid::now_v7();

        let mut peak = initial.clone();
        push_market_close(&mut peak, 240_000, price_at_r(Decimal::ONE));
        let protected = RsiEvaluator
            .evaluate(
                &profile,
                &peak,
                &created.next_state,
                &runner_feedback(signal_id, virtual_trade_id, 240_000, entry),
            )
            .unwrap();

        let mut first_adverse = initial.clone();
        push_market_close(&mut first_adverse, 300_000, price_at_r(Decimal::new(8, 1)));
        let watching = RsiEvaluator
            .evaluate(&profile, &first_adverse, &protected.next_state, &[])
            .unwrap();
        assert!(watching.directives.is_empty());

        let mut second_adverse = initial;
        push_market_close(&mut second_adverse, 360_000, price_at_r(Decimal::new(4, 1)));
        let exited = RsiEvaluator
            .evaluate(&profile, &second_adverse, &watching.next_state, &[])
            .unwrap();

        assert_eq!(exited.directives[0].kind, DirectiveKind::SuggestClose);
        assert_eq!(exited.directives[0].reason, "RSI_PROFIT_REVERSAL");
        assert_eq!(exited.directives[0].price, None);
        assert_eq!(
            exited.directives[0].effective_at,
            second_adverse.evaluated_closed_candle_time + 1
        );
    }
}

#[test]
fn profit_reversal_counter_resets_and_ignores_duplicate_candle() {
    let profile = profile(false);
    let initial = snapshot(Direction::Buy, false);
    let created = RsiEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let signal = created.next_state.active_signal.as_ref().unwrap().clone();
    let entry = Decimal::new(4_105, 0);
    let risk = (entry - signal.stop_loss).abs();
    let price_at_r = |r: Decimal| entry + risk * r;
    let virtual_trade_id = Uuid::now_v7();

    let mut peak = initial.clone();
    push_market_close(&mut peak, 240_000, price_at_r(Decimal::ONE));
    let protected = RsiEvaluator
        .evaluate(
            &profile,
            &peak,
            &created.next_state,
            &runner_feedback(signal.signal_id, virtual_trade_id, 240_000, entry),
        )
        .unwrap();

    let mut adverse = initial.clone();
    push_market_close(&mut adverse, 300_000, price_at_r(Decimal::new(8, 1)));
    let first_adverse = RsiEvaluator
        .evaluate(&profile, &adverse, &protected.next_state, &[])
        .unwrap();
    assert!(first_adverse.directives.is_empty());

    let duplicate = RsiEvaluator
        .evaluate(&profile, &adverse, &first_adverse.next_state, &[])
        .unwrap();
    assert!(duplicate.directives.is_empty());

    let mut recovered = initial.clone();
    push_market_close(&mut recovered, 360_000, price_at_r(Decimal::new(9, 1)));
    let reset = RsiEvaluator
        .evaluate(&profile, &recovered, &duplicate.next_state, &[])
        .unwrap();
    assert!(reset.directives.is_empty());

    let mut adverse_again = initial.clone();
    push_market_close(&mut adverse_again, 420_000, price_at_r(Decimal::new(6, 1)));
    let watching = RsiEvaluator
        .evaluate(&profile, &adverse_again, &reset.next_state, &[])
        .unwrap();
    assert!(watching.directives.is_empty());

    let mut confirmed = initial;
    push_market_close(&mut confirmed, 480_000, price_at_r(Decimal::new(4, 1)));
    let exited = RsiEvaluator
        .evaluate(&profile, &confirmed, &watching.next_state, &[])
        .unwrap();
    assert_eq!(exited.directives[0].kind, DirectiveKind::SuggestClose);
    assert_eq!(exited.directives[0].reason, "RSI_PROFIT_REVERSAL");
}

#[test]
fn profit_reversal_does_not_exit_at_non_positive_profit() {
    let profile = profile(false);
    let initial = snapshot(Direction::Buy, false);
    let created = RsiEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let signal = created.next_state.active_signal.as_ref().unwrap().clone();
    let entry = Decimal::new(4_105, 0);
    let risk = (entry - signal.stop_loss).abs();
    let virtual_trade_id = Uuid::now_v7();

    let mut peak = initial.clone();
    push_market_close(&mut peak, 240_000, entry + risk);
    let protected = RsiEvaluator
        .evaluate(
            &profile,
            &peak,
            &created.next_state,
            &runner_feedback(signal.signal_id, virtual_trade_id, 240_000, entry),
        )
        .unwrap();

    let mut first_adverse = initial.clone();
    push_market_close(&mut first_adverse, 300_000, entry + risk / Decimal::TWO);
    let watching = RsiEvaluator
        .evaluate(&profile, &first_adverse, &protected.next_state, &[])
        .unwrap();

    let mut below_entry = initial;
    push_market_close(&mut below_entry, 360_000, entry - risk / Decimal::TEN);
    let evaluation = RsiEvaluator
        .evaluate(&profile, &below_entry, &watching.next_state, &[])
        .unwrap();

    assert!(
        evaluation
            .directives
            .iter()
            .all(|directive| directive.reason != "RSI_PROFIT_REVERSAL")
    );
}

#[test]
fn plan_specific_exit_outranks_profit_reversal() {
    let profile = profile(false);
    let initial = snapshot(Direction::Buy, false);
    let created = RsiEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let signal = created.next_state.active_signal.as_ref().unwrap().clone();
    let entry = Decimal::new(4_105, 0);
    let risk = (entry - signal.stop_loss).abs();
    let virtual_trade_id = Uuid::now_v7();

    let mut peak = initial.clone();
    push_market_close(&mut peak, 240_000, entry + risk);
    let protected = RsiEvaluator
        .evaluate(
            &profile,
            &peak,
            &created.next_state,
            &runner_feedback(signal.signal_id, virtual_trade_id, 240_000, entry),
        )
        .unwrap();

    let mut first_adverse = initial.clone();
    push_market_close(
        &mut first_adverse,
        300_000,
        entry + risk * Decimal::new(8, 1),
    );
    let watching = RsiEvaluator
        .evaluate(&profile, &first_adverse, &protected.next_state, &[])
        .unwrap();

    let mut opposing = initial;
    push_rsi_and_close(
        &mut opposing,
        360_000,
        Decimal::new(70, 0),
        entry + risk * Decimal::new(4, 1),
    );
    let evaluation = RsiEvaluator
        .evaluate(&profile, &opposing, &watching.next_state, &[])
        .unwrap();

    assert_eq!(evaluation.directives.len(), 1);
    assert_eq!(evaluation.directives[0].reason, "RSI_OPPOSING_THRESHOLD");
}

#[test]
fn runner_extends_target_to_buffered_immutable_swing_after_all_gates_pass() {
    let profile = profile(false);
    let mut initial = snapshot(Direction::Buy, false);
    let far = ConfirmedSwing {
        id: id(FactKind::SwingHigh, 80, 180_001),
        kind: SwingKind::High,
        label: None,
        price: Decimal::new(4_250, 0),
        pivot_open_time: 160_000,
        confirmed_at: 180_001,
        source_start_time: 150_000,
        source_end_time: 180_001,
    };
    initial.target.facts.push(fact(far.id.clone(), far));
    let initial = with_htf_structure(initial, Direction::Buy);
    let created = RsiEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let state = created.next_state;
    let signal = state.active_signal.as_ref().unwrap().clone();
    let signal_id = signal.signal_id;
    let virtual_trade_id = Uuid::now_v7();
    let entry = Decimal::new(4_105, 0);
    let initial_risk = (entry - signal.stop_loss).abs();
    let target_distance = (signal.primary_target - entry).abs();

    let mut breakeven = initial.clone();
    push_market_close(&mut breakeven, 240_000, entry + initial_risk);
    let moved_stop = RsiEvaluator
        .evaluate(
            &profile,
            &breakeven,
            &state,
            &runner_feedback(signal_id, virtual_trade_id, 240_000, entry),
        )
        .unwrap();
    assert!(moved_stop.directives.is_empty());

    let mut near_target = initial;
    push_market_close(
        &mut near_target,
        300_000,
        entry + target_distance * Decimal::new(8, 1),
    );
    let extended = RsiEvaluator
        .evaluate(&profile, &near_target, &moved_stop.next_state, &[])
        .unwrap();

    assert_eq!(extended.directives.len(), 1);
    let directive = &extended.directives[0];
    assert_eq!(directive.kind, DirectiveKind::MoveTarget);
    assert_eq!(directive.reason, "RSI_SHARED_TARGET_EXTENSION");
    assert_eq!(directive.price, Some(Decimal::new(4_249, 0)));
    assert_eq!(directive.algorithm_version, signal.algorithm_version);
    assert_eq!(directive.parameter_version, signal.parameter_version);
    assert_eq!(
        extended
            .next_state
            .active_signal
            .as_ref()
            .unwrap()
            .primary_target,
        signal.primary_target
    );
}

#[test]
fn runner_target_extension_rejects_every_non_whitelisted_htf_context() {
    for context in [
        Some(HtfContext {
            higher_timeframe: Some(Timeframe::M5),
            direction: Direction::Sell,
            alignment: HtfAlignment::Counter,
            source: "STRUCTURE".to_owned(),
        }),
        Some(HtfContext {
            higher_timeframe: Some(Timeframe::M5),
            direction: Direction::None,
            alignment: HtfAlignment::Missing,
            source: "MISSING".to_owned(),
        }),
        Some(HtfContext {
            higher_timeframe: None,
            direction: Direction::None,
            alignment: HtfAlignment::Off,
            source: "OFF".to_owned(),
        }),
        None,
    ] {
        let profile = profile(false);
        let mut initial = snapshot(Direction::Buy, false);
        let far = ConfirmedSwing {
            id: id(FactKind::SwingHigh, 80, 180_001),
            kind: SwingKind::High,
            label: None,
            price: Decimal::new(4_250, 0),
            pivot_open_time: 160_000,
            confirmed_at: 180_001,
            source_start_time: 150_000,
            source_end_time: 180_001,
        };
        initial.target.facts.push(fact(far.id.clone(), far));
        let created = RsiEvaluator
            .evaluate(
                &profile,
                &initial,
                &PlanRuntimeState::neutral(&profile),
                &[],
            )
            .unwrap();
        let mut state = created.next_state;
        let signal = state.active_signal.as_mut().unwrap();
        signal.htf_context = context.clone();
        let primary_target = signal.primary_target;
        let signal_id = signal.signal_id;
        let virtual_trade_id = Uuid::now_v7();
        let entry = Decimal::new(4_105, 0);
        let initial_risk = (entry - signal.stop_loss).abs();
        let target_distance = (primary_target - entry).abs();
        let mut breakeven = initial.clone();
        push_market_close(&mut breakeven, 240_000, entry + initial_risk);
        let runner = RsiEvaluator
            .evaluate(
                &profile,
                &breakeven,
                &state,
                &runner_feedback(signal_id, virtual_trade_id, 240_000, entry),
            )
            .unwrap();
        let mut near_target = initial;
        push_market_close(
            &mut near_target,
            300_000,
            entry + target_distance * Decimal::new(8, 1),
        );

        let evaluation = RsiEvaluator
            .evaluate(&profile, &near_target, &runner.next_state, &[])
            .unwrap();

        assert!(
            evaluation
                .directives
                .iter()
                .all(|directive| { directive.kind != DirectiveKind::MoveTarget }),
            "context {context:?} must reject target extension"
        );
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
}

#[test]
fn runner_does_not_extend_target_without_plan_supplied_level() {
    let profile = profile(false);
    let initial = with_htf_structure(snapshot(Direction::Buy, false), Direction::Buy);
    let created = RsiEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let signal = created.next_state.active_signal.as_ref().unwrap();
    let signal_id = signal.signal_id;
    let virtual_trade_id = Uuid::now_v7();
    let entry = Decimal::new(4_105, 0);
    let mut progress = initial;
    push_market_close(&mut progress, 240_000, entry + Decimal::new(35, 0));

    let evaluation = RsiEvaluator
        .evaluate(
            &profile,
            &progress,
            &created.next_state,
            &runner_feedback(signal_id, virtual_trade_id, 240_000, entry),
        )
        .unwrap();

    assert!(
        evaluation
            .directives
            .iter()
            .all(|directive| { directive.kind != DirectiveKind::MoveTarget })
    );
}

#[test]
fn counter_htf_open_trade_does_not_extend_target() {
    let profile = profile(false);
    let mut base = snapshot(Direction::Buy, false);
    let far = ConfirmedSwing {
        id: id(FactKind::SwingHigh, 80, 180_001),
        kind: SwingKind::High,
        label: None,
        price: Decimal::new(4_250, 0),
        pivot_open_time: 160_000,
        confirmed_at: 180_001,
        source_start_time: 150_000,
        source_end_time: 180_001,
    };
    base.target.facts.push(fact(far.id.clone(), far));
    let initial = with_htf_structure(base, Direction::Sell);
    let created = RsiEvaluator
        .evaluate(
            &profile,
            &initial,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    let state = created.next_state;
    let signal = state.active_signal.as_ref().unwrap().clone();
    let signal_id = signal.signal_id;
    let virtual_trade_id = Uuid::now_v7();
    let entry = Decimal::new(4_105, 0);
    let initial_risk = (entry - signal.stop_loss).abs();
    let target_distance = (signal.primary_target - entry).abs();

    let mut breakeven = initial.clone();
    push_market_close(&mut breakeven, 240_000, entry + initial_risk);
    let moved_stop = RsiEvaluator
        .evaluate(
            &profile,
            &breakeven,
            &state,
            &runner_feedback(signal_id, virtual_trade_id, 240_000, entry),
        )
        .unwrap();
    assert_eq!(moved_stop.directives[0].kind, DirectiveKind::MoveStop);

    let mut near_target = initial;
    push_market_close(
        &mut near_target,
        300_000,
        entry + target_distance * Decimal::new(8, 1),
    );
    let evaluation = RsiEvaluator
        .evaluate(&profile, &near_target, &moved_stop.next_state, &[])
        .unwrap();

    assert!(evaluation.directives.is_empty());
}

#[test]
fn closed_trade_clears_active_signal_before_next_setup() {
    let profile = profile(false);
    let snapshot = snapshot(Direction::Buy, false);
    let mut state = RsiEvaluator
        .evaluate(
            &profile,
            &snapshot,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap()
        .next_state;
    state.trade_state = TradeState::Closed;
    state.active_virtual_trade_id = None;
    state.position_stage = PositionStage::Runner;
    state.trade_health = analysis_plans::TradeHealth::Invalid;
    state.remaining_fraction = Decimal::new(5, 1);
    state.realized_r = Decimal::new(5, 1);
    state.thesis_invalidation = Some(Decimal::new(4_080, 0));
    state.tp1_filled_at = Some(240_000);
    state.tp2_zone = state
        .active_signal
        .as_ref()
        .and_then(|signal| signal.execution_plan.as_ref())
        .map(|plan| plan.tp2_zone.clone());

    let evaluation = RsiEvaluator
        .evaluate(&profile, &snapshot, &state, &[])
        .unwrap();

    assert!(evaluation.next_state.active_signal.is_none());
    assert_eq!(evaluation.next_state.direction, Direction::None);
    assert_eq!(evaluation.next_state.setup_state, SetupState::Neutral);
    assert_eq!(evaluation.next_state.position_stage, PositionStage::Legacy);
    assert_eq!(
        evaluation.next_state.trade_health,
        analysis_plans::TradeHealth::Healthy
    );
    assert_eq!(evaluation.next_state.remaining_fraction, Decimal::ONE);
    assert_eq!(evaluation.next_state.realized_r, Decimal::ZERO);
    assert_eq!(evaluation.next_state.thesis_invalidation, None);
    assert_eq!(evaluation.next_state.tp1_filled_at, None);
    assert_eq!(evaluation.next_state.tp2_zone, None);
    assert!(evaluation.signal.is_none());
}

#[test]
fn unentered_signal_expires_at_its_versioned_boundary() {
    let profile = profile(false);
    let mut snapshot = snapshot(Direction::Buy, false);
    let created = RsiEvaluator
        .evaluate(
            &profile,
            &snapshot,
            &PlanRuntimeState::neutral(&profile),
            &[],
        )
        .unwrap();
    snapshot.evaluated_closed_candle_time = created
        .next_state
        .active_signal
        .as_ref()
        .unwrap()
        .expires_at
        .unwrap();
    let expired = RsiEvaluator
        .evaluate(&profile, &snapshot, &created.next_state, &[])
        .unwrap();
    assert_eq!(expired.directives[0].kind, DirectiveKind::ExpireEntry);
}
