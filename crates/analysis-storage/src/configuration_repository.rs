use crate::{storage_error, storage_sqlx_error};
use analysis_domain::{PlanKind, PlanProfileKey, ReleaseMode};
use analysis_service::{
    AnalysisServiceError, ConfigurationRelease, ConfigurationStore, MaterializedProfile,
    ValidationGate,
};
use async_trait::async_trait;
use market_domain::{Symbol, Timeframe};
use sqlx::{PgPool, Row};
use std::str::FromStr;
use uuid::Uuid;

#[derive(Clone)]
pub struct ConfigurationRepository {
    pool: PgPool,
}

impl ConfigurationRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl ConfigurationStore for ConfigurationRepository {
    async fn create_draft(
        &self,
        release: &ConfigurationRelease,
    ) -> Result<(), AnalysisServiceError> {
        if release.lifecycle.as_str() != "DRAFT" {
            return Err(AnalysisServiceError::InvalidData(
                "new configuration release must be DRAFT".to_owned(),
            ));
        }
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| storage_sqlx_error("begin configuration draft", error))?;
        sqlx::query(
            "INSERT INTO configuration_releases \
             (release_id, symbol, mode, lifecycle, validation_summary) \
             VALUES ($1, $2, $3, 'DRAFT', $4)",
        )
        .bind(release.release_id)
        .bind(release.symbol.as_str())
        .bind(release.mode.as_str())
        .bind(&release.validation_summary)
        .execute(&mut *transaction)
        .await
        .map_err(|error| storage_sqlx_error("insert configuration release", error))?;

        for profile in &release.profiles {
            if profile.key.symbol != release.symbol {
                return Err(AnalysisServiceError::InvalidData(
                    "profile symbol must match release symbol".to_owned(),
                ));
            }
            sqlx::query(
                "INSERT INTO analysis_profiles \
                 (release_id, symbol, plan, interval, enabled, parameter_version, \
                  algorithm_version, parameters, profit_protection, position_management, required_timeframes, \
                  minimum_history_bars, warmup_bars) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
            )
            .bind(release.release_id)
            .bind(profile.key.symbol.as_str())
            .bind(profile.key.plan.as_str())
            .bind(profile.key.target_timeframe.as_str())
            .bind(profile.enabled)
            .bind(&profile.versions.parameter_version)
            .bind(&profile.versions.algorithm_version)
            .bind(
                serde_json::to_value(&profile.parameters)
                    .map_err(|error| storage_error("serialize plan parameters", error))?,
            )
            .bind(
                serde_json::to_value(&profile.profit_protection)
                    .map_err(|error| storage_error("serialize profit protection", error))?,
            )
            .bind(
                serde_json::to_value(&profile.position_management)
                    .map_err(|error| storage_error("serialize position management", error))?,
            )
            .bind(
                serde_json::to_value(&profile.required_timeframes)
                    .map_err(|error| storage_error("serialize required timeframes", error))?,
            )
            .bind(i32::try_from(profile.minimum_history_bars).map_err(|_| {
                AnalysisServiceError::InvalidData("minimum history exceeds i32".to_owned())
            })?)
            .bind(i32::try_from(profile.warmup_bars).map_err(|_| {
                AnalysisServiceError::InvalidData("warmup bars exceeds i32".to_owned())
            })?)
            .execute(&mut *transaction)
            .await
            .map_err(|error| storage_sqlx_error("insert analysis profile", error))?;
        }

        transaction
            .commit()
            .await
            .map_err(|error| storage_sqlx_error("commit configuration draft", error))
    }

    async fn activate(
        &self,
        release_id: Uuid,
        gate: &ValidationGate,
    ) -> Result<(), AnalysisServiceError> {
        let mut transaction = self
            .pool
            .begin()
            .await
            .map_err(|error| storage_sqlx_error("begin configuration activation", error))?;
        let row = sqlx::query(
            "SELECT symbol, mode, lifecycle FROM configuration_releases \
             WHERE release_id = $1 FOR UPDATE",
        )
        .bind(release_id)
        .fetch_optional(&mut *transaction)
        .await
        .map_err(|error| storage_sqlx_error("lock configuration release", error))?
        .ok_or_else(|| AnalysisServiceError::NotFound(release_id.to_string()))?;
        let symbol: String = row.get("symbol");
        let mode: String = row.get("mode");
        let lifecycle: String = row.get("lifecycle");
        if lifecycle != "DRAFT" {
            return Err(AnalysisServiceError::Conflict(
                "only a draft release can be activated".to_owned(),
            ));
        }
        if mode == ReleaseMode::PaperActive.as_str()
            && (!gate.chronological_split
                || !gate.out_of_sample_complete
                || gate.minimum_sample_size == 0
                || gate.sample_size < gate.minimum_sample_size
                || gate.unresolved_gap_count != 0
                || gate
                    .metrics
                    .as_object()
                    .is_none_or(serde_json::Map::is_empty)
                || gate.evaluation_start < 0
                || gate.evaluation_end <= gate.evaluation_start)
        {
            return Err(AnalysisServiceError::InvalidData(
                "PAPER_ACTIVE activation requires validation evidence".to_owned(),
            ));
        }
        sqlx::query(
            "UPDATE configuration_releases SET lifecycle = 'RETIRED' \
             WHERE symbol = $1 AND lifecycle = 'ACTIVE'",
        )
        .bind(&symbol)
        .execute(&mut *transaction)
        .await
        .map_err(|error| storage_sqlx_error("retire active configuration", error))?;
        sqlx::query(
            "UPDATE configuration_releases \
             SET lifecycle = 'ACTIVE', validation_summary = $2, activated_at = NOW() \
             WHERE release_id = $1",
        )
        .bind(release_id)
        .bind(
            serde_json::to_value(gate)
                .map_err(|error| storage_error("serialize validation gate", error))?,
        )
        .execute(&mut *transaction)
        .await
        .map_err(|error| storage_sqlx_error("activate configuration release", error))?;
        transaction
            .commit()
            .await
            .map_err(|error| storage_sqlx_error("commit configuration activation", error))
    }

    async fn active_profiles(
        &self,
        symbol: &Symbol,
    ) -> Result<Vec<MaterializedProfile>, AnalysisServiceError> {
        let rows = sqlx::query(
            "SELECT p.release_id, p.symbol, p.plan, p.interval, p.enabled, p.parameter_version, \
                    p.algorithm_version, p.parameters, p.profit_protection, p.position_management, p.required_timeframes, \
                    p.minimum_history_bars, p.warmup_bars \
             FROM analysis_profiles p \
             JOIN configuration_releases r ON r.release_id = p.release_id \
             WHERE p.symbol = $1 AND r.lifecycle = 'ACTIVE' \
             ORDER BY p.plan, p.interval",
        )
        .bind(symbol.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load active analysis profiles", error))?;

        rows.into_iter().map(row_to_profile).collect()
    }

    async fn release_mode(&self, release_id: Uuid) -> Result<ReleaseMode, AnalysisServiceError> {
        let mode: String =
            sqlx::query_scalar("SELECT mode FROM configuration_releases WHERE release_id = $1")
                .bind(release_id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|error| storage_sqlx_error("load configuration release mode", error))?
                .ok_or_else(|| AnalysisServiceError::NotFound(release_id.to_string()))?;
        match mode.as_str() {
            "RESEARCH" => Ok(ReleaseMode::Research),
            "PAPER_ACTIVE" => Ok(ReleaseMode::PaperActive),
            _ => Err(AnalysisServiceError::InvalidData(format!(
                "unknown release mode {mode}"
            ))),
        }
    }

    async fn profiles_for_release(
        &self,
        release_id: Uuid,
        symbol: &Symbol,
    ) -> Result<Vec<MaterializedProfile>, AnalysisServiceError> {
        let rows = sqlx::query(
            "SELECT p.release_id, p.symbol, p.plan, p.interval, p.enabled, p.parameter_version, \
                    p.algorithm_version, p.parameters, p.profit_protection, p.position_management, p.required_timeframes, \
                    p.minimum_history_bars, p.warmup_bars \
             FROM analysis_profiles p \
             WHERE p.release_id = $1 AND p.symbol = $2 \
             ORDER BY p.plan, p.interval",
        )
        .bind(release_id)
        .bind(symbol.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|error| storage_sqlx_error("load release analysis profiles", error))?;
        rows.into_iter().map(row_to_profile).collect()
    }
}

fn row_to_profile(row: sqlx::postgres::PgRow) -> Result<MaterializedProfile, AnalysisServiceError> {
    let symbol_text: String = row.get("symbol");
    let plan_text: String = row.get("plan");
    let interval: String = row.get("interval");
    let required_timeframes: serde_json::Value = row.get("required_timeframes");
    Ok(MaterializedProfile {
        key: PlanProfileKey {
            symbol: Symbol::new(symbol_text)
                .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?,
            plan: parse_plan(&plan_text)?,
            target_timeframe: Timeframe::from_str(&interval)
                .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?,
        },
        enabled: row.get("enabled"),
        versions: analysis_domain::VersionIdentity::new(
            row.get("release_id"),
            row.get::<String, _>("algorithm_version"),
            row.get::<String, _>("parameter_version"),
        )
        .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?,
        parameters: serde_json::from_value(row.get("parameters"))
            .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?,
        profit_protection: serde_json::from_value(row.get("profit_protection"))
            .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?,
        position_management: serde_json::from_value(row.get("position_management"))
            .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?,
        required_timeframes: serde_json::from_value(required_timeframes)
            .map_err(|error| AnalysisServiceError::InvalidData(error.to_string()))?,
        minimum_history_bars: usize::try_from(row.get::<i32, _>("minimum_history_bars")).map_err(
            |_| AnalysisServiceError::InvalidData("negative minimum history".to_owned()),
        )?,
        warmup_bars: usize::try_from(row.get::<i32, _>("warmup_bars"))
            .map_err(|_| AnalysisServiceError::InvalidData("negative warmup".to_owned()))?,
    })
}

pub(crate) fn parse_plan(value: &str) -> Result<PlanKind, AnalysisServiceError> {
    match value {
        "ICT" => Ok(PlanKind::Ict),
        "SMC" => Ok(PlanKind::Smc),
        "RSI" => Ok(PlanKind::Rsi),
        "QUASIMODO" => Ok(PlanKind::Quasimodo),
        "TREND_BREAKOUT" => Ok(PlanKind::TrendBreakout),
        "MEAN_REVERSION" => Ok(PlanKind::MeanReversion),
        _ => Err(AnalysisServiceError::InvalidData(format!(
            "unknown plan {value}"
        ))),
    }
}
