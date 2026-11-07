use super::*;

const EXPECTED: &str = include_str!("../../tests/fixtures/runtime-profile-catalog.json");

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeProfileContract {
    pub plan: String,
    pub timeframe: String,
    pub enabled: bool,
    pub algorithm_version: String,
    pub parameter_version: String,
    pub parameters: serde_json::Value,
    pub profit_protection: serde_json::Value,
    pub position_management: serde_json::Value,
    pub required_timeframes: Vec<String>,
    pub minimum_history_bars: usize,
    pub warmup_bars: usize,
}

fn profile_contract_rows(symbol: Symbol) -> Vec<RuntimeProfileContract> {
    let release_id = uuid::Uuid::nil();
    let catalog = RuntimeProfileCatalog::current().unwrap();
    let mut profiles = Vec::with_capacity(catalog.entries().len());
    for plan in PlanKind::ALL {
        for timeframe in Timeframe::ALL {
            let profile = catalog
                .profile(release_id, symbol.clone(), plan, timeframe)
                .unwrap();
            profiles.push((
                plan,
                timeframe,
                RuntimeProfileContract {
                    plan: plan.as_str().to_owned(),
                    timeframe: timeframe.as_str().to_owned(),
                    enabled: profile.enabled,
                    algorithm_version: profile.versions.algorithm_version,
                    parameter_version: profile.versions.parameter_version,
                    parameters: serde_json::to_value(profile.parameters).unwrap(),
                    profit_protection: serde_json::to_value(profile.profit_protection).unwrap(),
                    position_management: serde_json::to_value(profile.position_management).unwrap(),
                    required_timeframes: profile
                        .required_timeframes
                        .into_iter()
                        .map(|required| required.as_str().to_owned())
                        .collect(),
                    minimum_history_bars: profile.minimum_history_bars,
                    warmup_bars: profile.warmup_bars,
                },
            ));
        }
    }
    profiles.sort_by_key(|(plan, timeframe, _)| (*plan, timeframe.duration_ms()));
    profiles.into_iter().map(|(_, _, row)| row).collect()
}

#[test]
fn runtime_profile_contract_matches_fixture() {
    let registry = super::runtime_plan_registry();
    assert!(
        PlanKind::ALL
            .into_iter()
            .all(|plan| registry.get(plan).is_some())
    );

    let rows = profile_contract_rows(Symbol::new("XAUUSDT").unwrap());
    assert_eq!(rows.len(), PlanKind::ALL.len() * Timeframe::ALL.len());

    let unique = rows
        .iter()
        .map(|row| (&row.plan, &row.timeframe))
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(unique.len(), rows.len());

    let expected: serde_json::Value = serde_json::from_str(EXPECTED).unwrap();
    let actual = serde_json::to_value(rows).unwrap();
    assert_eq!(actual, expected);
}

#[test]
fn runtime_profile_catalog_contains_exactly_one_profile_per_plan_and_timeframe() {
    let catalog = RuntimeProfileCatalog::current().unwrap();
    assert_eq!(catalog.entries().len(), 42);
    catalog.validate().unwrap();
}

#[test]
fn daily_smc_disables_unavailable_higher_timeframe_filter() {
    let catalog = RuntimeProfileCatalog::current().unwrap();
    for timeframe in Timeframe::ALL {
        let profile = catalog
            .profile(
                uuid::Uuid::nil(),
                Symbol::new("XAUUSDT").unwrap(),
                PlanKind::Smc,
                timeframe,
            )
            .unwrap();
        let analysis_domain::PlanParameters::Smc(parameters) = profile.parameters else {
            unreachable!()
        };
        if timeframe == Timeframe::D1 {
            assert_eq!(parameters.higher_timeframe, None);
            assert_eq!(parameters.htf_mode, analysis_domain::HtfFilterMode::Off);
            assert!(profile.required_timeframes.is_empty());
            assert_eq!(
                profile.versions.parameter_version,
                "SMC-1d-runtime-v3-pp1-pc1"
            );
        } else {
            assert_eq!(
                parameters.higher_timeframe,
                next_higher_timeframe(timeframe)
            );
            assert_ne!(parameters.htf_mode, analysis_domain::HtfFilterMode::Off);
        }
    }
}

#[test]
fn stage_a_runtime_identity_remains_rsi_v4() {
    let release_id = uuid::Uuid::nil();
    let profile = RuntimeProfileCatalog::current()
        .unwrap()
        .profile(
            release_id,
            Symbol::new("XAUUSDT").unwrap(),
            PlanKind::Rsi,
            Timeframe::M5,
        )
        .unwrap();
    assert_eq!(profile.versions.algorithm_version, "RSI-runtime-v4");
    assert!(
        runtime_plan_registry()
            .get(PlanKind::Rsi)
            .is_some_and(|evaluator| evaluator.algorithm_version() == "rsi-v4")
    );
}

#[test]
fn runtime_profile_catalog_fingerprint_is_deterministic() {
    let catalog = RuntimeProfileCatalog::current().unwrap();
    let keys = catalog
        .entries()
        .iter()
        .map(|entry| (entry.plan, entry.timeframe.duration_ms()))
        .collect::<Vec<_>>();
    let mut sorted_keys = keys.clone();
    sorted_keys.sort();
    assert_eq!(keys, sorted_keys);
    assert_eq!(
        catalog.fingerprint(),
        serde_json::to_string(catalog.entries()).unwrap()
    );
    assert_eq!(
        catalog.fingerprint(),
        RuntimeProfileCatalog::current().unwrap().fingerprint()
    );
}
