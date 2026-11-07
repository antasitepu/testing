use crate::{ServiceError, SessionRecord, SessionSource};
use chrono::{Datelike, Duration, LocalResult, NaiveDate, NaiveTime, TimeZone, Utc, Weekday};
use chrono_tz::America::New_York;
use market_domain::{Candle, MarketSession, SessionStatus, Timeframe};

const WEEKLY_POLICY_REFERENCE: &str = "commodity-weekly-v2";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandleSessionClass {
    Tradable,
    NoTrading,
    Unknown,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct TradableCandlePolicy;

impl TradableCandlePolicy {
    pub fn classify(&self, candle: &Candle, sessions: &[MarketSession]) -> CandleSessionClass {
        let Some(end) = candle.close_time.checked_add(1) else {
            return CandleSessionClass::Unknown;
        };
        self.classify_range(candle.open_time, end, sessions)
    }

    pub fn classify_range(
        &self,
        start_time: i64,
        end_time: i64,
        sessions: &[MarketSession],
    ) -> CandleSessionClass {
        if end_time <= start_time {
            return CandleSessionClass::Unknown;
        }
        let mut cursor = start_time;
        let mut has_regular = false;
        for session in sessions {
            if session.end_time <= cursor || session.start_time >= end_time {
                continue;
            }
            if session.start_time > cursor {
                return CandleSessionClass::Unknown;
            }
            has_regular |= session.status == SessionStatus::Regular;
            cursor = cursor.max(session.end_time.min(end_time));
            if cursor == end_time {
                return if has_regular {
                    CandleSessionClass::Tradable
                } else {
                    CandleSessionClass::NoTrading
                };
            }
        }
        CandleSessionClass::Unknown
    }

    /// The open of the first bar at or after `open_time` that this policy trades. `None` when
    /// session coverage runs out first, so nothing is assumed about the uncovered time.
    pub fn first_tradable_open(
        &self,
        timeframe: Timeframe,
        open_time: i64,
        sessions: &[MarketSession],
    ) -> Option<i64> {
        let mut open = open_time;
        loop {
            let end = open.checked_add(timeframe.duration_ms())?;
            match self.classify_range(open, end, sessions) {
                CandleSessionClass::Tradable => return Some(open),
                CandleSessionClass::NoTrading => open = end,
                CandleSessionClass::Unknown => return None,
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CommoditySessionGenerator;

impl CommoditySessionGenerator {
    pub fn generate(
        &self,
        start_time: i64,
        end_time: i64,
        source_event_time: i64,
    ) -> Result<Vec<SessionRecord>, ServiceError> {
        if end_time <= start_time {
            return Err(ServiceError::InvalidData(
                "session generation end must be later than start".to_owned(),
            ));
        }
        let start = Utc
            .timestamp_millis_opt(start_time)
            .single()
            .ok_or_else(|| ServiceError::InvalidData("invalid generation start".to_owned()))?;
        let end = Utc
            .timestamp_millis_opt(end_time - 1)
            .single()
            .ok_or_else(|| ServiceError::InvalidData("invalid generation end".to_owned()))?;
        let mut date = start.with_timezone(&New_York).date_naive() - Duration::days(3);
        let last_date = end.with_timezone(&New_York).date_naive() + Duration::days(1);
        let mut closures = Vec::new();
        while date <= last_date {
            // Binance's COMMODITY schedule closes only for the weekend, with no daily break.
            if date.weekday() == Weekday::Fri {
                let sunday = date + Duration::days(2);
                closures.push((local_timestamp(date, 17)?, local_timestamp(sunday, 18)?));
            }
            date = date.succ_opt().ok_or_else(|| {
                ServiceError::InvalidData("session generation date overflow".to_owned())
            })?;
        }
        closures.sort_unstable();
        let mut clipped: Vec<(i64, i64)> = Vec::new();
        for (start, end) in closures {
            let start = start.max(start_time);
            let end = end.min(end_time);
            if start >= end {
                continue;
            }
            if let Some(last) = clipped.last_mut()
                && start <= last.1
            {
                last.1 = last.1.max(end);
            } else {
                clipped.push((start, end));
            }
        }

        let mut records = Vec::new();
        let mut cursor = start_time;
        for (closure_start, closure_end) in clipped {
            if cursor < closure_start {
                records.push(derived_record(
                    SessionStatus::Regular,
                    cursor,
                    closure_start,
                    source_event_time,
                )?);
            }
            records.push(derived_record(
                SessionStatus::NoTrading,
                closure_start,
                closure_end,
                source_event_time,
            )?);
            cursor = closure_end;
        }
        if cursor < end_time {
            records.push(derived_record(
                SessionStatus::Regular,
                cursor,
                end_time,
                source_event_time,
            )?);
        }
        Ok(records)
    }
}

pub fn canonicalize_sessions(
    start_time: i64,
    end_time: i64,
    records: &[SessionRecord],
) -> Result<Vec<SessionRecord>, ServiceError> {
    if end_time <= start_time {
        return Err(ServiceError::InvalidData(
            "canonical session end must be later than start".to_owned(),
        ));
    }
    let mut boundaries = vec![start_time, end_time];
    for record in records {
        if record.session.end_time <= start_time || record.session.start_time >= end_time {
            continue;
        }
        boundaries.push(record.session.start_time.max(start_time));
        boundaries.push(record.session.end_time.min(end_time));
    }
    boundaries.sort_unstable();
    boundaries.dedup();

    let mut canonical: Vec<SessionRecord> = Vec::new();
    for window in boundaries.windows(2) {
        let segment_start = window[0];
        let segment_end = window[1];
        let covering = records
            .iter()
            .filter(|record| {
                record.session.start_time <= segment_start && record.session.end_time >= segment_end
            })
            .collect::<Vec<_>>();
        let Some(max_precedence) = covering
            .iter()
            .map(|record| record.source.precedence())
            .max()
        else {
            return Err(ServiceError::InvalidData(format!(
                "missing canonical session coverage at {segment_start}"
            )));
        };
        let winners = covering
            .into_iter()
            .filter(|record| record.source.precedence() == max_precedence)
            .collect::<Vec<_>>();
        let winner = winners.iter().copied().max_by_key(|record| {
            (
                record.session.source_event_time,
                record.source_reference.as_deref().unwrap_or_default(),
            )
        });
        let Some(winner) = winner else {
            return Err(ServiceError::InvalidData(
                "canonical session winner is missing".to_owned(),
            ));
        };
        if winners
            .iter()
            .any(|record| record.session.status != winner.session.status)
        {
            return Err(ServiceError::InvalidData(format!(
                "equal-precedence session conflict at {segment_start}"
            )));
        }
        let segment = SessionRecord {
            session: MarketSession::new(
                winner.session.session_market.clone(),
                winner.session.status,
                segment_start,
                segment_end,
                winner.session.source_event_time,
            )
            .map_err(|error| ServiceError::InvalidData(error.to_string()))?,
            source: winner.source,
            source_reference: winner.source_reference.clone(),
        };
        if let Some(previous) = canonical.last_mut()
            && same_metadata(previous, &segment)
            && previous.session.end_time == segment.session.start_time
        {
            previous.session.end_time = segment.session.end_time;
            previous.session.source_event_time = previous
                .session
                .source_event_time
                .max(segment.session.source_event_time);
        } else {
            canonical.push(segment);
        }
    }
    Ok(canonical)
}

fn local_timestamp(date: NaiveDate, hour: u32) -> Result<i64, ServiceError> {
    let local =
        date.and_time(NaiveTime::from_hms_opt(hour, 0, 0).ok_or_else(|| {
            ServiceError::InvalidData(format!("invalid local session hour {hour}"))
        })?);
    match New_York.from_local_datetime(&local) {
        LocalResult::Single(value) => Ok(value.timestamp_millis()),
        LocalResult::Ambiguous(_, _) => Err(ServiceError::InvalidData(format!(
            "ambiguous local session boundary {local}"
        ))),
        LocalResult::None => Err(ServiceError::InvalidData(format!(
            "missing local session boundary {local}"
        ))),
    }
}

fn derived_record(
    status: SessionStatus,
    start_time: i64,
    end_time: i64,
    source_event_time: i64,
) -> Result<SessionRecord, ServiceError> {
    Ok(SessionRecord {
        session: MarketSession::new("COMMODITY", status, start_time, end_time, source_event_time)
            .map_err(|error| ServiceError::InvalidData(error.to_string()))?,
        source: SessionSource::DerivedWeekly,
        source_reference: Some(WEEKLY_POLICY_REFERENCE.to_owned()),
    })
}

fn same_metadata(left: &SessionRecord, right: &SessionRecord) -> bool {
    left.session.session_market == right.session.session_market
        && left.session.status == right.session.status
        && left.source == right.source
        && left.source_reference == right.source_reference
}
