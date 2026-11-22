//! Transfer reports backed by the transfers database.

use crate::{app::AppState, auth::Authenticated, db::Database};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::get,
};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rusqlite::Row;
use serde::Deserialize;
use slskr_api::telemetry::{
    LeaderboardSortBy, SortOrder, TransferDirectorySummary, TransferExceptionDetail,
    TransferExceptionSummary, TransferHistogram, TransferLeaderboard, TransferSummary,
    TransferSummaryByState, TransferSummaryReport, UserDirectionTransferStatistics,
    UserDirectionTransferSummary, UserTransferReport,
};
use slskr_api::transfers::TransferStates;

const COMPLETED_STATES: &str = "16, 48, 80, 144, 272, 528, 1040";
// Reference genesis (2020-12-30 06:22 UTC) and DateTime's year 1–9999 bounds.
const GENESIS: DateTime<Utc> = utc(1_609_309_320, 0);
const MIN_TIME: DateTime<Utc> = utc(-62_135_596_800, 0);
const MAX_TIME: DateTime<Utc> = utc(253_402_300_799, 999_999_999);

const fn utc(seconds: i64, nanos: u32) -> DateTime<Utc> {
    match DateTime::from_timestamp(seconds, nanos) {
        Some(time) => time,
        None => panic!("invalid report timestamp"),
    }
}

fn timestamp(time: DateTime<Utc>) -> String {
    // SQLite compares the TEXT bounds with the transfer store's fixed-width UTC timestamps.
    time.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn datetime(row: &Row<'_>, index: usize) -> rusqlite::Result<Option<DateTime<Utc>>> {
    row.get::<_, Option<String>>(index)?
        .map(|value| {
            value.parse().map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Text,
                    Box::new(error),
                )
            })
        })
        .transpose()
}

fn summary_state(bits: u32) -> String {
    TransferStates::from_bits(bits)
        .difference(TransferStates::COMPLETED)
        .to_string()
}

#[derive(Clone)]
pub struct ReportsService {
    database: Database,
}

impl ReportsService {
    pub fn new(database: Database) -> Self {
        Self { database }
    }

    async fn oldest_requested(&self) -> anyhow::Result<Option<DateTime<Utc>>> {
        self.database
            .call(|db| {
                db.query_row("SELECT MIN(requested_at) FROM transfers", [], |row| {
                    datetime(row, 0)
                })
            })
            .await
    }

    pub async fn summary(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        direction: Option<&str>,
        username: Option<&str>,
    ) -> anyhow::Result<TransferSummaryReport> {
        let (start, end) = (timestamp(start), timestamp(end));
        let direction = direction.map(str::to_owned);
        let username = username.map(str::to_owned);
        self.database.call(move |db| {
            let mut stmt = db.prepare(&format!(
                "SELECT direction, state, SUM(size), COUNT(*), COUNT(DISTINCT username), AVG(average_speed),
                    AVG(strftime('%s', started_at)-strftime('%s', enqueued_at)),
                    AVG(strftime('%s', ended_at)-strftime('%s', started_at))
                 FROM transfers WHERE ended_at BETWEEN ?1 AND ?2
                    AND state IN ({COMPLETED_STATES}) AND (?3 IS NULL OR direction = ?3)
                    AND (?4 IS NULL OR username = ?4) GROUP BY direction, state"
            ))?;
            let mut rows = stmt.query(rusqlite::params![start, end, direction, username])?;
            let mut report = empty_summary();
            while let Some(row) = rows.next()? {
                let direction: String = row.get(0)?;
                let state = summary_state(row.get(1)?);
                let record = TransferSummary {
                    total_bytes: row.get(2)?, count: row.get(3)?, distinct_users: row.get(4)?,
                    average_speed: row.get::<_, Option<f64>>(5)?.unwrap_or_default(),
                    average_wait: row.get::<_, Option<f64>>(6)?.unwrap_or_default(),
                    average_duration: row.get::<_, Option<f64>>(7)?.unwrap_or_default(),
                    ..Default::default()
                };
                report.entry(direction).or_default().insert(state, record);
            }
            Ok(report)
        }).await
    }

    pub async fn histogram(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        interval_seconds: i64,
        direction: Option<&str>,
        username: Option<&str>,
    ) -> anyhow::Result<TransferHistogram> {
        anyhow::ensure!(start < end, "End time must be later than start time");
        let seconds = interval_seconds.max(1);
        let begin = start.timestamp() / seconds * seconds;
        let (start_text, end_text) = (timestamp(start), timestamp(end));
        let direction = direction.map(str::to_owned);
        let username = username.map(str::to_owned);
        self.database.call(move |db| {
            let mut histogram = TransferHistogram::new();
            let mut bucket = begin;
            while let Some(at) = DateTime::from_timestamp(bucket, 0).filter(|&at| at < end) {
                histogram.insert(at, empty_summary());
                bucket += seconds;
            }
            let mut stmt = db.prepare(&format!(
                "SELECT (CAST(strftime('%s', ended_at) AS INTEGER) / ?1) * ?1, direction, state,
                    SUM(size), COUNT(*), COUNT(DISTINCT username),
                    CAST(SUM(size) AS REAL) / NULLIF(SUM(strftime('%s', ended_at)-strftime('%s', started_at)), 0),
                    AVG(strftime('%s', started_at)-strftime('%s', enqueued_at)),
                    AVG(strftime('%s', ended_at)-strftime('%s', started_at))
                 FROM transfers WHERE ended_at BETWEEN ?2 AND ?3
                    AND state IN ({COMPLETED_STATES}) AND (?4 IS NULL OR direction = ?4)
                    AND (?5 IS NULL OR username = ?5) GROUP BY 1, 2, 3"
            ))?;
            let mut rows = stmt.query(rusqlite::params![seconds, start_text, end_text, direction, username])?;
            while let Some(row) = rows.next()? {
                let Some(summary) = DateTime::from_timestamp(row.get(0)?, 0)
                    .and_then(|at| histogram.get_mut(&at))
                else {
                    continue;
                };
                summary.entry(row.get(1)?).or_default().insert(summary_state(row.get(2)?), TransferSummary {
                    total_bytes: row.get(3)?, count: row.get(4)?, distinct_users: row.get(5)?,
                    average_speed: row.get::<_, Option<f64>>(6)?.unwrap_or_default(),
                    average_wait: row.get::<_, Option<f64>>(7)?.unwrap_or_default(),
                    average_duration: row.get::<_, Option<f64>>(8)?.unwrap_or_default(),
                    ..Default::default()
                });
            }
            Ok(histogram)
        }).await
    }

    pub async fn leaderboard(
        &self,
        direction: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        sort: LeaderboardSortBy,
        order: SortOrder,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<TransferLeaderboard> {
        let dir = direction.to_owned();
        let (start, end) = (timestamp(start), timestamp(end));
        let field = match sort {
            LeaderboardSortBy::Count => "count",
            LeaderboardSortBy::TotalBytes => "total_bytes",
            LeaderboardSortBy::AverageSpeed => "average_speed",
        };
        let ordering = if order == SortOrder::ASC {
            "ASC"
        } else {
            "DESC"
        };
        self.database.call(move |db| {
            let sql = format!(
                "SELECT username, SUM(size) AS total_bytes, COUNT(*) AS count, AVG(average_speed) AS average_speed,
                    AVG(strftime('%s', started_at)-strftime('%s', enqueued_at)),
                    AVG(strftime('%s', ended_at)-strftime('%s', started_at))
                 FROM transfers WHERE direction = ?1 AND state = 48 AND ended_at BETWEEN ?2 AND ?3
                 GROUP BY username ORDER BY {field} {ordering}, username DESC LIMIT ?4 OFFSET ?5"
            );
            let mut stmt = db.prepare(&sql)?;
            let mut rows = stmt.query(rusqlite::params![dir, start, end, limit, offset])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? { out.push(TransferSummary { username: row.get(0)?, total_bytes: row.get(1)?, count: row.get(2)?, average_speed: row.get::<_, Option<f64>>(3)?.unwrap_or_default(), average_wait: row.get::<_, Option<f64>>(4)?.unwrap_or_default(), average_duration: row.get::<_, Option<f64>>(5)?.unwrap_or_default(), ..Default::default() }); }
            Ok(out)
        }).await
    }

    pub async fn exceptions(
        &self,
        direction: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        username: Option<&str>,
        order: SortOrder,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<TransferExceptionDetail>> {
        let (direction, start, end, username) = (
            direction.to_owned(),
            timestamp(start),
            timestamp(end),
            username.map(str::to_owned),
        );
        self.database.call(move |db| {
            let sql = format!("SELECT id, username, direction, filename, size, state, requested_at, enqueued_at, started_at, ended_at, bytes_transferred, average_speed, CASE WHEN instr(exception, ':') > 0 THEN substr(exception, instr(exception, ':') + 2) ELSE exception END FROM transfers WHERE (state & ~48) != 0 AND direction=?1 AND ended_at IS NOT NULL AND ended_at >= ?2 AND ended_at <= ?3 AND (?4 IS NULL OR username=?4) ORDER BY ended_at {} LIMIT ?5 OFFSET ?6", if order == SortOrder::ASC { "ASC" } else { "DESC" });
            let mut stmt = db.prepare(&sql)?;
            let mut rows = stmt.query(rusqlite::params![direction, start, end, username, limit, offset])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(TransferExceptionDetail {
                    id: row.get(0)?, username: row.get(1)?, direction: row.get(2)?, filename: row.get(3)?,
                    size: row.get(4)?, state: TransferStates::from_bits(row.get(5)?).to_string(),
                    requested_at: datetime(row, 6)?.ok_or(rusqlite::Error::InvalidQuery)?,
                    enqueued_at: datetime(row, 7)?, started_at: datetime(row, 8)?, ended_at: datetime(row, 9)?,
                    bytes_transferred: row.get(10)?, average_speed: row.get(11)?, exception: row.get(12)?,
                });
            }
            Ok(out)
        }).await
    }

    pub async fn exception_pareto(
        &self,
        direction: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        username: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<TransferExceptionSummary>> {
        let (direction, start, end, username) = (
            direction.to_owned(),
            timestamp(start),
            timestamp(end),
            username.map(str::to_owned),
        );
        self.database.call(move |db| {
            let mut stmt = db.prepare("SELECT CASE WHEN instr(exception, ':') > 0 THEN substr(exception, instr(exception, ':') + 2) ELSE exception END, COUNT(*), COUNT(DISTINCT username) FROM transfers WHERE (state & ~48) != 0 AND direction=?1 AND ended_at IS NOT NULL AND ended_at >= ?2 AND ended_at <= ?3 AND (?4 IS NULL OR username=?4) GROUP BY 1 ORDER BY COUNT(*) DESC, 1 LIMIT ?5 OFFSET ?6")?;
            let mut rows = stmt.query(rusqlite::params![direction, start, end, username, limit, offset])?;
            let mut out = Vec::new();
            while let Some(r) = rows.next()? { out.push(TransferExceptionSummary { exception: r.get(0)?, count: r.get(1)?, distinct_users: r.get(2)? }); }
            Ok(out)
        }).await
    }

    pub async fn user_report(
        &self,
        username: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> anyhow::Result<UserTransferReport> {
        let mut result = UserTransferReport::new();
        let mut summary = self.summary(start, end, None, Some(username)).await?;
        for direction in ["Upload", "Download"] {
            let by_state = summary.remove(direction).unwrap_or_default();
            let total = by_state
                .values()
                .fold(0_i64, |sum, item| sum.saturating_add(item.count));
            let successful = by_state.get("Succeeded").map_or(0, |item| item.count);
            let errored = ["Errored", "TimedOut", "Rejected", "Aborted"]
                .iter()
                .filter_map(|state| by_state.get(*state))
                .fold(0_i64, |sum, item| sum.saturating_add(item.count));
            let cancelled = by_state.get("Cancelled").map_or(0, |item| item.count);
            let exceptions = self
                .exception_pareto(direction, start, end, Some(username), 25, 0)
                .await?;
            result.insert(
                direction.to_owned(),
                UserDirectionTransferSummary {
                    summary: by_state,
                    statistics: UserDirectionTransferStatistics {
                        total,
                        successful,
                        errored,
                        cancelled,
                    },
                    exceptions,
                },
            );
        }
        Ok(result)
    }

    pub async fn directories(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        username: Option<&str>,
        limit: i64,
        offset: i64,
    ) -> anyhow::Result<Vec<TransferDirectorySummary>> {
        let (start, end, username) = (
            timestamp(start),
            timestamp(end),
            username.map(str::to_owned),
        );
        self.database.call(move |db| {
            let mut stmt = db.prepare("WITH dirs AS (SELECT rtrim(rtrim(filename, replace(replace(filename,'/',''),'\\','')), '/\\') AS directory, username FROM transfers WHERE direction='Upload' AND state=48 AND ended_at IS NOT NULL AND ended_at >= ?1 AND ended_at <= ?2 AND (?3 IS NULL OR username=?3)) SELECT directory, COUNT(*), COUNT(DISTINCT username) FROM dirs GROUP BY directory ORDER BY COUNT(DISTINCT username) DESC, COUNT(*) DESC, directory LIMIT ?4 OFFSET ?5")?;
            let mut rows = stmt.query(rusqlite::params![start, end, username, limit, offset])?;
            let mut out = Vec::new();
            while let Some(r) = rows.next()? { out.push(TransferDirectorySummary { directory: r.get(0)?, count: r.get(1)?, distinct_users: r.get(2)? }); }
            Ok(out)
        }).await
    }
}

fn empty_summary() -> TransferSummaryReport {
    ["Download", "Upload"]
        .into_iter()
        .map(|d| (d.to_owned(), TransferSummaryByState::new()))
        .collect()
}

type ReportError = (StatusCode, Json<String>);

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Window {
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    direction: Option<String>,
    username: Option<String>,
    limit: Option<i32>,
    offset: Option<i32>,
    sort_by: Option<String>,
    sort_order: Option<String>,
    buckets: Option<i32>,
    interval: Option<i32>,
}

impl Window {
    fn bounds(
        &self,
        default_start: DateTime<Utc>,
        default_end: DateTime<Utc>,
    ) -> Result<(DateTime<Utc>, DateTime<Utc>), ReportError> {
        let start = self.start.unwrap_or(default_start);
        let end = self.end.unwrap_or(default_end);
        if start >= end {
            return Err(bad_request("End time must be later than start time"));
        }
        Ok((start, end))
    }

    fn recent_bounds(&self) -> Result<(DateTime<Utc>, DateTime<Utc>), ReportError> {
        let now = Utc::now();
        self.bounds(now - Duration::days(7), now)
    }

    fn direction(&self) -> Result<Option<&'static str>, ReportError> {
        let Some(value) = self
            .direction
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(None);
        };
        let numeric = value.parse::<i32>();
        if value.eq_ignore_ascii_case("Download") || numeric == Ok(0) {
            Ok(Some("Download"))
        } else if value.eq_ignore_ascii_case("Upload") || numeric == Ok(1) {
            Ok(Some("Upload"))
        } else {
            Err(bad_request(
                "Invalid direction; expected one of: Download, Upload",
            ))
        }
    }

    fn required_direction(&self) -> Result<&'static str, ReportError> {
        self.direction()?
            .ok_or_else(|| bad_request("Direction is required"))
    }

    fn username(&self) -> Option<&str> {
        self.username
            .as_deref()
            .filter(|value| !value.trim().is_empty())
    }

    fn sort_by(&self) -> Result<LeaderboardSortBy, ReportError> {
        let Some(value) = self
            .sort_by
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(LeaderboardSortBy::Count);
        };
        match value.parse::<i32>() {
            Ok(0) => Ok(LeaderboardSortBy::Count),
            Ok(1) => Ok(LeaderboardSortBy::TotalBytes),
            Ok(2) => Ok(LeaderboardSortBy::AverageSpeed),
            _ => LeaderboardSortBy::parse(value).ok_or_else(|| {
                bad_request("Invalid sortBy; expected one of: Count, TotalBytes, AverageSpeed")
            }),
        }
    }

    fn sort_order(&self) -> Result<SortOrder, ReportError> {
        let Some(value) = self
            .sort_order
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return Ok(SortOrder::DESC);
        };
        match value.parse::<i32>() {
            Ok(0) => Ok(SortOrder::ASC),
            Ok(1) => Ok(SortOrder::DESC),
            _ => SortOrder::parse(value)
                .ok_or_else(|| bad_request("Invalid sortOrder; expected one of: ASC, DESC")),
        }
    }

    fn paging(&self) -> Result<(i64, i64), ReportError> {
        let limit = self.limit.unwrap_or(25);
        let offset = self.offset.unwrap_or(0);
        if limit <= 0 {
            return Err(bad_request("Limit must be greater than zero"));
        }
        if offset < 0 {
            return Err(bad_request("Offset must be greater than or equal to zero"));
        }
        Ok((limit.into(), offset.into()))
    }

    fn histogram_size(&self) -> Result<HistogramSize, ReportError> {
        match (self.interval, self.buckets) {
            (Some(_), Some(_)) => Err(bad_request("Specify either interval or buckets, not both")),
            (Some(minutes), None) if minutes <= 1 => {
                Err(bad_request("Interval must be greater than 1"))
            }
            (Some(minutes), None) => Ok(HistogramSize::Seconds(i64::from(minutes) * 60)),
            (None, Some(buckets)) if !(1..=1000).contains(&buckets) => Err(bad_request(
                "Buckets must be between 1 and 1000, if specified",
            )),
            (None, buckets) => Ok(HistogramSize::Buckets(i64::from(buckets.unwrap_or(100)))),
        }
    }
}

enum HistogramSize {
    Seconds(i64),
    Buckets(i64),
}

impl HistogramSize {
    fn seconds(self, start: DateTime<Utc>, end: DateTime<Utc>) -> i64 {
        match self {
            Self::Seconds(seconds) => seconds,
            Self::Buckets(count) => (end.signed_duration_since(start).num_seconds() / count).max(1),
        }
    }
}

fn bad_request(message: &str) -> ReportError {
    (StatusCode::BAD_REQUEST, Json(message.to_owned()))
}

fn fail(error: impl std::fmt::Display) -> ReportError {
    (StatusCode::INTERNAL_SERVER_ERROR, Json(error.to_string()))
}

async fn summary(
    _: Authenticated,
    State(state): State<AppState>,
    Query(q): Query<Window>,
) -> Result<Json<TransferSummaryReport>, ReportError> {
    let (start, end) = q.recent_bounds()?;
    let direction = q.direction()?;
    state
        .services
        .telemetry
        .reports
        .summary(start, end, direction, q.username())
        .await
        .map(Json)
        .map_err(fail)
}

async fn histogram(
    _: Authenticated,
    State(state): State<AppState>,
    Query(q): Query<Window>,
) -> Result<Json<TransferHistogram>, ReportError> {
    let (mut start, end) = q.recent_bounds()?;
    if end < GENESIS {
        return Err(bad_request(
            "End time impossibly early (earlier than slskd genesis time 2020-12-30 06:22 UTC)",
        ));
    }
    let size = q.histogram_size()?;
    let direction = q.direction()?;
    let reports = &state.services.telemetry.reports;
    if start < GENESIS {
        start = reports
            .oldest_requested()
            .await
            .map_err(fail)?
            .unwrap_or(GENESIS);
    }
    reports
        .histogram(
            start,
            end,
            size.seconds(start, end),
            direction,
            q.username(),
        )
        .await
        .map(Json)
        .map_err(fail)
}

async fn leaderboard(
    _: Authenticated,
    State(state): State<AppState>,
    Query(q): Query<Window>,
) -> Result<Json<TransferLeaderboard>, ReportError> {
    let direction = q.required_direction()?;
    let (start, end) = q.bounds(GENESIS, Utc::now())?;
    let sort = q.sort_by()?;
    let order = q.sort_order()?;
    let (limit, offset) = q.paging()?;
    state
        .services
        .telemetry
        .reports
        .leaderboard(direction, start, end, sort, order, limit, offset)
        .await
        .map(Json)
        .map_err(fail)
}

async fn exceptions(
    _: Authenticated,
    State(state): State<AppState>,
    Query(q): Query<Window>,
) -> Result<Json<Vec<TransferExceptionDetail>>, ReportError> {
    let direction = q.required_direction()?;
    let (start, end) = q.bounds(MIN_TIME, MAX_TIME)?;
    let order = q.sort_order()?;
    let (limit, offset) = q.paging()?;
    state
        .services
        .telemetry
        .reports
        .exceptions(direction, start, end, q.username(), order, limit, offset)
        .await
        .map(Json)
        .map_err(fail)
}

async fn pareto(
    _: Authenticated,
    State(state): State<AppState>,
    Query(q): Query<Window>,
) -> Result<Json<Vec<TransferExceptionSummary>>, ReportError> {
    let direction = q.required_direction()?;
    let (start, end) = q.bounds(MIN_TIME, MAX_TIME)?;
    let (limit, offset) = q.paging()?;
    state
        .services
        .telemetry
        .reports
        .exception_pareto(direction, start, end, q.username(), limit, offset)
        .await
        .map(Json)
        .map_err(fail)
}

async fn user_report(
    _: Authenticated,
    Path(username): Path<String>,
    State(state): State<AppState>,
    Query(q): Query<Window>,
) -> Result<Json<UserTransferReport>, ReportError> {
    if username.trim().is_empty() {
        return Err(bad_request("Username is required"));
    }
    let (start, end) = q.bounds(GENESIS, Utc::now())?;
    state
        .services
        .telemetry
        .reports
        .user_report(&username, start, end)
        .await
        .map(Json)
        .map_err(fail)
}

async fn directories(
    _: Authenticated,
    State(state): State<AppState>,
    Query(q): Query<Window>,
) -> Result<Json<Vec<TransferDirectorySummary>>, ReportError> {
    let (start, end) = q.bounds(GENESIS, MAX_TIME)?;
    let (limit, offset) = q.paging()?;
    state
        .services
        .telemetry
        .reports
        .directories(start, end, q.username(), limit, offset)
        .await
        .map(Json)
        .map_err(fail)
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/telemetry/reports/transfers/summary", get(summary))
        .route("/telemetry/reports/transfers/histogram", get(histogram))
        .route("/telemetry/reports/transfers/leaderboard", get(leaderboard))
        .route("/telemetry/reports/transfers/exceptions", get(exceptions))
        .route(
            "/telemetry/reports/transfers/exceptions/pareto",
            get(pareto),
        )
        .route("/telemetry/reports/transfers/directories", get(directories))
        .route(
            "/telemetry/reports/transfers/users/{username}",
            get(user_report),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: DateTime<Utc> = utc(1_767_225_600, 0);
    const END: DateTime<Utc> = utc(1_767_225_720, 250_000_000);

    #[test]
    fn dashboard_queries_bind_camel_case_and_use_minutes() {
        let uri = "/?direction=dOwNlOaD&sortBy=TotalBytes&sortOrder=ASC&interval=2"
            .parse()
            .unwrap();
        let Query(query) = Query::<Window>::try_from_uri(&uri).unwrap();
        assert_eq!(query.required_direction().unwrap(), "Download");
        assert_eq!(query.sort_by().unwrap(), LeaderboardSortBy::TotalBytes);
        assert_eq!(query.sort_order().unwrap(), SortOrder::ASC);
        assert_eq!(query.histogram_size().unwrap().seconds(START, END), 120);

        let uri = "/?direction=neither&sortOrder=asc&interval=2&buckets=1"
            .parse()
            .unwrap();
        let Query(query) = Query::<Window>::try_from_uri(&uri).unwrap();
        assert!(query.direction().is_err());
        assert!(query.sort_order().is_err());
        assert!(query.histogram_size().is_err());
        let query = Window {
            buckets: Some(5),
            ..Window::default()
        };
        assert_eq!(query.histogram_size().unwrap().seconds(START, END), 24);
    }

    fn fixture() -> ReportsService {
        let database = Database::open(std::path::Path::new(":memory:"), &[r#"
            CREATE TABLE transfers (
                id TEXT, username TEXT, direction TEXT, filename TEXT, size INTEGER, state INTEGER,
                requested_at TEXT, enqueued_at TEXT, started_at TEXT, ended_at TEXT,
                bytes_transferred INTEGER, average_speed REAL, exception TEXT
            );
            INSERT INTO transfers VALUES
                ('d1','alice','Download','one.flac',120,48,
                 '2025-12-31T23:59:57.000000000Z','2025-12-31T23:59:57.000000000Z',
                 '2025-12-31T23:59:58.000000000Z','2026-01-01T00:00:00.000000000Z',120,12,NULL),
                ('d2','bob','Download','two.flac',600,48,
                 '2026-01-01T00:01:57.000000000Z','2026-01-01T00:01:57.000000000Z',
                 '2026-01-01T00:01:58.000000000Z','2026-01-01T00:02:00.250000000Z',600,6,NULL),
                ('d3','alice','Download','bad.flac',90,272,
                 '2026-01-01T00:00:00.000000000Z',NULL,NULL,'2026-01-01T00:01:00.000000000Z',0,0,'IOException: refused'),
                ('d4','alice','Download','out.flac',900,48,
                 '2026-01-01T00:01:57.000000000Z',NULL,
                 '2026-01-01T00:01:58.000000000Z','2026-01-01T00:02:00.250000001Z',900,18,NULL),
                ('u1','alice','Upload','music\album\one.flac',40,48,
                 '2026-01-01T00:00:00.000000000Z',NULL,NULL,'2026-01-01T00:01:00.000000000Z',40,4,NULL),
                ('u2','bob','Upload','music\album\two.flac',70,48,
                 '2026-01-01T00:00:00.000000000Z',NULL,NULL,'2026-01-01T00:01:01.000000000Z',70,7,NULL);
        "#]).unwrap();
        ReportsService::new(database)
    }

    #[tokio::test]
    async fn summary_and_histogram_match_direction_state_and_window() {
        let reports = fixture();
        let summary = reports
            .summary(START, END, Some("Download"), None)
            .await
            .unwrap();
        let succeeded = &summary["Download"]["Succeeded"];
        assert_eq!(
            (
                succeeded.count,
                succeeded.total_bytes,
                succeeded.distinct_users
            ),
            (2, 720, 2)
        );
        assert_eq!(succeeded.average_speed, 9.0);
        assert_eq!(succeeded.average_wait, 1.0);
        assert_eq!(summary["Download"]["Errored"].count, 1);
        assert!(summary["Upload"].is_empty());

        let histogram = reports
            .histogram(START, END, 120, Some("Download"), None)
            .await
            .unwrap();
        assert_eq!(histogram.len(), 2);
        let final_bucket = &histogram[&(START + Duration::minutes(2))];
        assert_eq!(final_bucket["Download"]["Succeeded"].average_speed, 300.0);
        assert!(final_bucket["Upload"].is_empty());
    }

    #[tokio::test]
    async fn dashboard_lists_and_user_details_use_transfer_history() {
        let reports = fixture();
        let top = reports
            .leaderboard(
                "Download",
                START,
                END,
                LeaderboardSortBy::TotalBytes,
                SortOrder::DESC,
                1,
                0,
            )
            .await
            .unwrap();
        assert_eq!(top[0].username.as_deref(), Some("bob"));
        let top = reports
            .leaderboard(
                "Download",
                START,
                END,
                LeaderboardSortBy::AverageSpeed,
                SortOrder::DESC,
                1,
                0,
            )
            .await
            .unwrap();
        assert_eq!(top[0].username.as_deref(), Some("alice"));

        let details = reports
            .exceptions("Download", MIN_TIME, MAX_TIME, None, SortOrder::DESC, 25, 0)
            .await
            .unwrap();
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].state, "Completed, Errored");
        assert_eq!(details[0].exception.as_deref(), Some("refused"));
        assert_eq!(details[0].requested_at, START);
        let user = reports.user_report("alice", START, END).await.unwrap();
        assert_eq!(user["Download"].statistics.total, 2);
        assert_eq!(user["Download"].statistics.successful, 1);
        assert_eq!(user["Download"].statistics.errored, 1);
        assert_eq!(
            user["Download"].exceptions[0].exception.as_deref(),
            Some("refused")
        );
        assert_eq!(user["Upload"].statistics.successful, 1);

        let dirs = reports.directories(START, END, None, 25, 0).await.unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].directory.as_deref(), Some(r"music\album"));
        assert_eq!((dirs[0].count, dirs[0].distinct_users), (2, 2));
    }
}
