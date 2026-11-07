use crate::{ConfigurationRelease, MaterializedProfile, ValidationGate};
use analysis_domain::{PlanKind, ReleaseMode};
use std::collections::HashSet;

pub fn validate_release(
    release: &ConfigurationRelease,
) -> Result<(), ConfigurationValidationError> {
    if release.profiles.len() != PlanKind::ALL.len() * 7 {
        return Err(ConfigurationValidationError::IncompleteProfileSet);
    }
    let mut keys = HashSet::with_capacity(release.profiles.len());
    for profile in &release.profiles {
        validate_profile(release, profile)?;
        if !keys.insert(profile.key.clone()) {
            return Err(ConfigurationValidationError::DuplicateProfile);
        }
    }
    Ok(())
}

pub fn validate_activation(
    release: &ConfigurationRelease,
    gate: &ValidationGate,
) -> Result<(), ConfigurationValidationError> {
    validate_release(release)?;
    if release.mode == ReleaseMode::Research {
        return Ok(());
    }
    if !gate.chronological_split
        || !gate.out_of_sample_complete
        || gate.minimum_sample_size == 0
        || gate.sample_size < gate.minimum_sample_size
        || gate.unresolved_gap_count != 0
        || gate
            .metrics
            .as_object()
            .is_none_or(serde_json::Map::is_empty)
        || gate.evaluation_start < 0
        || gate.evaluation_end <= gate.evaluation_start
    {
        return Err(ConfigurationValidationError::PromotionGateFailed);
    }
    Ok(())
}

fn validate_profile(
    release: &ConfigurationRelease,
    profile: &MaterializedProfile,
) -> Result<(), ConfigurationValidationError> {
    if profile.key.symbol != release.symbol
        || profile.versions.release_id != release.release_id
        || profile.parameters.plan() != profile.key.plan
    {
        return Err(ConfigurationValidationError::ProfileIdentityMismatch);
    }
    if profile.minimum_history_bars == 0 {
        return Err(ConfigurationValidationError::InvalidHistoryRequirement);
    }
    profile
        .parameters
        .validate()
        .map_err(|error| ConfigurationValidationError::InvalidParameters(error.to_string()))?;
    profile
        .profit_protection
        .validate()
        .map_err(|error| ConfigurationValidationError::InvalidParameters(error.to_string()))?;
    profile
        .position_management
        .validate()
        .map_err(|error| ConfigurationValidationError::InvalidParameters(error.to_string()))?;

    let target_duration = profile.key.target_timeframe.duration_ms();
    if profile
        .required_timeframes
        .iter()
        .any(|timeframe| timeframe.duration_ms() <= target_duration)
    {
        return Err(ConfigurationValidationError::InvalidHigherTimeframe);
    }
    if let Some(higher_timeframe) = profile.parameters.higher_timeframe()
        && (higher_timeframe.duration_ms() <= target_duration
            || !profile.required_timeframes.contains(&higher_timeframe))
    {
        return Err(ConfigurationValidationError::InvalidHigherTimeframe);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum ConfigurationValidationError {
    #[error("configuration release must contain all 42 plan/timeframe profiles")]
    IncompleteProfileSet,
    #[error("configuration release contains a duplicate profile key")]
    DuplicateProfile,
    #[error("profile identity or parameter variant does not match its release key")]
    ProfileIdentityMismatch,
    #[error("minimum history bars must be positive")]
    InvalidHistoryRequirement,
    #[error("profile parameters are invalid: {0}")]
    InvalidParameters(String),
    #[error("configured higher timeframe must be above and included by the target profile")]
    InvalidHigherTimeframe,
    #[error("PAPER_ACTIVE validation gate failed")]
    PromotionGateFailed,
}
