use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub enum Timeframe {
    #[serde(rename = "1m")]
    M1,
    #[serde(rename = "5m")]
    M5,
    #[serde(rename = "15m")]
    M15,
    #[serde(rename = "30m")]
    M30,
    #[serde(rename = "1h")]
    H1,
    #[serde(rename = "4h")]
    H4,
    #[serde(rename = "1d")]
    D1,
}

impl Timeframe {
    pub const ALL: [Self; 7] = [
        Self::M1,
        Self::M5,
        Self::M15,
        Self::M30,
        Self::H1,
        Self::H4,
        Self::D1,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::M1 => "1m",
            Self::M5 => "5m",
            Self::M15 => "15m",
            Self::M30 => "30m",
            Self::H1 => "1h",
            Self::H4 => "4h",
            Self::D1 => "1d",
        }
    }

    pub const fn duration_ms(self) -> i64 {
        match self {
            Self::M1 => 60_000,
            Self::M5 => 300_000,
            Self::M15 => 900_000,
            Self::M30 => 1_800_000,
            Self::H1 => 3_600_000,
            Self::H4 => 14_400_000,
            Self::D1 => 86_400_000,
        }
    }
}

impl fmt::Display for Timeframe {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for Timeframe {
    type Err = TimeframeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "1m" => Ok(Self::M1),
            "5m" => Ok(Self::M5),
            "15m" => Ok(Self::M15),
            "30m" => Ok(Self::M30),
            "1h" => Ok(Self::H1),
            "4h" => Ok(Self::H4),
            "1d" => Ok(Self::D1),
            _ => Err(TimeframeParseError(value.to_owned())),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
#[error("unsupported timeframe: {0}")]
pub struct TimeframeParseError(String);
