//! Transfer reports backed by the transfers database.

mod cache;
pub(crate) use cache::cache_response;

use crate::http::{ApiError, ApiResult, Json, Path, Query, Routes};
use crate::{app::AppState, auth::Authenticated, db::Database};
// Transfer times are stored as nanoseconds since the epoch (see the transfer store).
use crate::transfers::{stored_date as datetime, stored_id, stored_time as timestamp};
use axum::extract::State;
use chrono::{DateTime, Duration, Utc};
use serde::Deserialize;
use slskr_types::telemetry::{
    LeaderboardSortBy, SortOrder, TransferDirectorySummary, TransferErrorDetail,
    TransferErrorSummary, TransferHistogram, TransferLeaderboard, TransferSummary,
    TransferSummaryByState, TransferSummaryReport, UserDirectionTransferStatistics,
    UserDirectionTransferSummary, UserTransferReport,
};
use slskr_types::transfers::{TransferDirection, TransferState};

/// The completed transfer state bits, spliced into constant SQL with `concat!`.
macro_rules! completed_states {
    () => {
        "16, 48, 80, 144, 272, 528, 1040"
    };
}
// Reference genesis (2020-12-30 06:22 UTC) and DateTime's year 1–9999 bounds.
const GENESIS: DateTime<Utc> = utc(1_609_309_320, 0);
const MIN_TIME: DateTime<Utc> = utc(-62_135_596_800, 0);
const MAX_TIME: DateTime<Utc> = utc(253_402_300_799, 999_999_999);
/// Upper bound on histogram buckets, a guard for the bucket width computed from
/// `buckets` (at most 1000) and the window.
const MAX_HISTOGRAM_BUCKETS: i64 = 10_000;

/// First bucket start, snapped to the Unix epoch like the SQL bucket formula.
fn histogram_begin(start: DateTime<Utc>, seconds: i64) -> i64 {
    start.timestamp() / seconds * seconds
}

/// Number of `seconds`-wide buckets that start before `end`.
fn histogram_bucket_count(start: DateTime<Utc>, end: DateTime<Utc>, seconds: i64) -> i64 {
    let seconds = seconds.max(1);
    let end = end.timestamp() + i64::from(end.timestamp_subsec_nanos() > 0);
    let span = (end - histogram_begin(start, seconds)).max(0);
    (span + seconds - 1) / seconds
}

fn empty_histogram(start: DateTime<Utc>, end: DateTime<Utc>, seconds: i64) -> TransferHistogram {
    let mut histogram = TransferHistogram::new();
    let mut bucket = histogram_begin(start, seconds);
    while let Some(at) = DateTime::from_timestamp(bucket, 0).filter(|&at| at < end) {
        histogram.insert(at, empty_summary());
        bucket += seconds;
    }
    histogram
}

const fn utc(seconds: i64, nanos: u32) -> DateTime<Utc> {
    match DateTime::from_timestamp(seconds, nanos) {
        Some(time) => time,
        None => panic!("invalid report timestamp"),
    }
}

fn summary_state(bits: u32) -> TransferState {
    crate::transfers::state_from_bits(bits)
}

/// The `transfers.direction` column value.
fn db_direction(direction: TransferDirection) -> &'static str {
    match direction {
        TransferDirection::Download => "Download",
        TransferDirection::Upload => "Upload",
    }
}

fn api_direction(direction: &str) -> TransferDirection {
    if direction == "Upload" {
        TransferDirection::Upload
    } else {
        TransferDirection::Download
    }
}

/// Whole milliseconds of an average in seconds.
fn ms(seconds: Option<f64>) -> u64 {
    (seconds.unwrap_or_default().max(0.0) * 1000.0).round() as u64
}

/// A report window split at whole hours: the hourly rollups cover `hours` (start
/// inclusive, end exclusive, in epoch seconds) and the raw transfers cover the partial
/// hours at either edge, each an inclusive `ended_at` range.
struct Split {
    hours: (i64, i64),
    edges: [(i64, i64); 2],
}

impl Split {
    const HOUR: i64 = 3600;

    fn new(start: DateTime<Utc>, end: DateTime<Utc>) -> Self {
        let first = start.timestamp().div_euclid(Self::HOUR) * Self::HOUR;
        let full_start = if start.timestamp() == first && start.timestamp_subsec_nanos() == 0 {
            first
        } else {
            first + Self::HOUR
        };
        let full_end = end.timestamp().div_euclid(Self::HOUR) * Self::HOUR;
        // Stored times are nanoseconds, clamped like `timestamp` outside i64.
        let nanos = |seconds: i64| {
            seconds
                .checked_mul(1_000_000_000)
                .unwrap_or(if seconds < 0 { i64::MIN } else { i64::MAX })
        };
        let (start, end) = (timestamp(start), timestamp(end));
        Self {
            hours: (full_start, full_end),
            // Each edge has a single lower and upper bound for the date index.
            edges: [
                (start, end.min(nanos(full_start).saturating_sub(1))),
                (nanos(full_start.max(full_end)), end),
            ],
        }
    }

    /// Binds `?1` to `?6`.
    fn params(&self) -> [i64; 6] {
        let Self { hours, edges } = self;
        [
            hours.0, hours.1, edges[0].0, edges[0].1, edges[1].0, edges[1].1,
        ]
    }
}

/// The rows of a split window: `rollup` over whole hours, then `raw` (the same columns,
/// computed per transfer from `table`, `transfers` or `transfer_details` when it needs
/// the texts) over the edges where `filter` holds. Binds `?1` to `?6`.
fn windowed(rollup: &str, raw: &str, table: &str, filter: &str) -> String {
    format!(
        "{rollup} WHERE hour >= ?1 AND hour < ?2
         UNION ALL {raw} FROM {table} WHERE ended_at BETWEEN ?3 AND ?4 AND {filter}
         UNION ALL {raw} FROM {table} WHERE ended_at BETWEEN ?5 AND ?6 AND {filter}"
    )
}

/// Completed transfers by direction, state and user (`transfer_user_hours`).
fn user_rows() -> String {
    windowed(
        "SELECT direction, state, username, files, bytes, speed, wait, waits, duration, durations
         FROM transfer_user_hours",
        "SELECT direction, state, username, 1, size, average_speed,
            started_at - enqueued_at + 0.0, started_at - enqueued_at IS NOT NULL,
            ended_at - started_at + 0.0, ended_at - started_at IS NOT NULL",
        "transfers",
        concat!("state IN (", completed_states!(), ")"),
    )
}

/// [`user_rows`] with the hour first.
fn user_hour_rows() -> String {
    windowed(
        "SELECT hour, direction, state, username, files, bytes, wait, waits, duration, durations
         FROM transfer_user_hours",
        "SELECT ended_at / 3600000000000 * 3600, direction, state, username, 1, size,
            started_at - enqueued_at + 0.0, started_at - enqueued_at IS NOT NULL,
            ended_at - started_at + 0.0, ended_at - started_at IS NOT NULL",
        "transfers",
        concat!("state IN (", completed_states!(), ")"),
    )
}

/// The averages over [`user_rows`]: speed, then wait and duration in seconds.
const USER_AVERAGES: &str = "SUM(speed) / SUM(files),
    SUM(wait) / NULLIF(SUM(waits), 0) / 1e9, SUM(duration) / NULLIF(SUM(durations), 0) / 1e9";

/// A failed transfer's error ('' for none), as `transfer_error_hours` keeps it.
const ERROR_TEXT: &str = "coalesce(exception, '')";

/// One page of a report list: at most `limit` rows after skipping `offset`.
#[derive(Clone, Copy, Debug)]
pub struct Page {
    pub limit: i64,
    pub offset: i64,
}

impl Page {
    /// The first `limit` rows.
    const fn first(limit: i64) -> Self {
        Self { limit, offset: 0 }
    }
}

#[derive(Clone)]
pub struct ReportsService {
    database: Database,
    cache: cache::ReportCache,
}

impl ReportsService {
    pub fn new(database: Database) -> Self {
        Self {
            database,
            cache: cache::ReportCache::default(),
        }
    }

    async fn oldest_requested(&self) -> anyhow::Result<Option<DateTime<Utc>>> {
        self.database
            .call(|db| {
                // The first stored transfer, or the first rollup hour: removed rows
                // are deleted after a while, but their hours stay.
                db.query_row(
                    "SELECT min(coalesce(first, hour), coalesce(hour, first)) FROM
                        (SELECT (SELECT requested_at FROM transfers ORDER BY rowid LIMIT 1) AS first,
                            (SELECT MIN(hour) * 1000000000 FROM transfer_chart_hours) AS hour)",
                    [],
                    |row| datetime(row, 0),
                )
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
        let split = Split::new(start, end);
        let direction = direction.map(str::to_owned);
        let username = username.map(str::to_owned);
        self.database.call(move |db| {
            let mut stmt = db.prepare_cached(&format!(
                "SELECT direction, state, SUM(bytes), SUM(files), COUNT(DISTINCT username), {USER_AVERAGES}
                 FROM ({}) WHERE (?7 IS NULL OR direction = ?7) AND (?8 IS NULL OR username = ?8)
                 GROUP BY direction, state",
                user_rows()
            ))?;
            let [a, b, c, d, e, f] = split.params();
            let mut rows = stmt.query(rusqlite::params![a, b, c, d, e, f, direction, username])?;
            let mut report = empty_summary();
            while let Some(row) = rows.next()? {
                let direction = api_direction(&row.get::<_, String>(0)?);
                let state = summary_state(row.get(1)?);
                let record = TransferSummary {
                    total_bytes: row.get(2)?, count: row.get(3)?, distinct_users: row.get(4)?,
                    average_speed: row.get::<_, Option<f64>>(5)?.unwrap_or_default(),
                    average_wait_ms: ms(row.get(6)?),
                    average_duration_ms: ms(row.get(7)?),
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
        anyhow::ensure!(
            histogram_bucket_count(start, end, seconds) <= MAX_HISTOGRAM_BUCKETS,
            "Histogram would exceed {MAX_HISTOGRAM_BUCKETS} buckets"
        );
        let split = Split::new(start, end);
        let direction = direction.map(str::to_owned);
        let username = username.map(str::to_owned);
        let mut histogram = empty_histogram(start, end, seconds);
        self.database
            .call(move |db| {
                // Whole-hour buckets read the hourly rollups, like the chart; others the
                // transfers, which only short windows use.
                let (rows, bucket) = if seconds % Split::HOUR == 0 {
                    (user_hour_rows(), "hour")
                } else {
                    (
                        format!(
                            "SELECT ended_at / 1000000000 AS second, direction, state, username, 1 AS files,
                                size AS bytes, started_at - enqueued_at + 0.0 AS wait,
                                started_at - enqueued_at IS NOT NULL AS waits,
                                ended_at - started_at + 0.0 AS duration,
                                ended_at - started_at IS NOT NULL AS durations
                             FROM transfers WHERE ended_at BETWEEN ?3 AND ?6 AND state IN ({})",
                            completed_states!()
                        ),
                        "second",
                    )
                };
                let mut stmt = db.prepare_cached(&format!(
                    "SELECT {bucket} / ?7 * ?7, direction, state, SUM(bytes), SUM(files),
                        COUNT(DISTINCT username), CAST(SUM(bytes) AS REAL) / (NULLIF(SUM(duration), 0) / 1e9),
                        SUM(wait) / NULLIF(SUM(waits), 0) / 1e9,
                        SUM(duration) / NULLIF(SUM(durations), 0) / 1e9
                     FROM ({rows}) WHERE (?8 IS NULL OR direction = ?8) AND (?9 IS NULL OR username = ?9)
                     GROUP BY 1, 2, 3"
                ))?;
                let [a, b, c, d, e, f] = split.params();
                let mut rows = stmt.query(rusqlite::params![
                    a, b, c, d, e, f, seconds, direction, username
                ])?;
                while let Some(row) = rows.next()? {
                    let Some(summary) = DateTime::from_timestamp(row.get(0)?, 0)
                        .and_then(|at| histogram.get_mut(&at))
                    else {
                        continue;
                    };
                    summary
                        .entry(api_direction(&row.get::<_, String>(1)?))
                        .or_default()
                        .insert(
                            summary_state(row.get(2)?),
                            TransferSummary {
                                total_bytes: row.get(3)?,
                                count: row.get(4)?,
                                distinct_users: row.get(5)?,
                                average_speed: row.get::<_, Option<f64>>(6)?.unwrap_or_default(),
                                average_wait_ms: ms(row.get(7)?),
                                average_duration_ms: ms(row.get(8)?),
                                ..Default::default()
                            },
                        );
                }
                Ok(histogram)
            })
            .await
    }

    /// Fast chart totals from completed hours, with the raw rows at the two edges.
    async fn chart(
        &self,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        seconds: i64,
    ) -> anyhow::Result<TransferHistogram> {
        let split = Split::new(start, end);
        self.database
            .call(move |db| {
                let mut histogram = empty_histogram(start, end, seconds);
                let mut stmt = db.prepare_cached(&format!(
                    "SELECT hour / ?7 * ?7, direction, state, SUM(bytes), SUM(files) FROM ({})
                     GROUP BY 1, 2, 3",
                    windowed(
                        "SELECT hour, direction, state, bytes, files FROM transfer_chart_hours",
                        "SELECT ended_at / 3600000000000 * 3600, direction, state, size, 1",
                        "transfers",
                        concat!("state IN (", completed_states!(), ")"),
                    )
                ))?;
                let [a, b, c, d, e, f] = split.params();
                let mut rows = stmt.query(rusqlite::params![a, b, c, d, e, f, seconds])?;
                while let Some(row) = rows.next()? {
                    let Some(summary) = DateTime::from_timestamp(row.get(0)?, 0)
                        .and_then(|at| histogram.get_mut(&at))
                    else {
                        continue;
                    };
                    summary
                        .entry(api_direction(&row.get::<_, String>(1)?))
                        .or_default()
                        .insert(
                            summary_state(row.get(2)?),
                            TransferSummary {
                                total_bytes: row.get(3)?,
                                count: row.get(4)?,
                                ..Default::default()
                            },
                        );
                }
                Ok(histogram)
            })
            .await
    }

    pub async fn leaderboard(
        &self,
        direction: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        sort: LeaderboardSortBy,
        order: SortOrder,
        page: Page,
    ) -> anyhow::Result<TransferLeaderboard> {
        let Page { limit, offset } = page;
        let dir = direction.to_owned();
        let split = Split::new(start, end);
        let field = match sort {
            LeaderboardSortBy::Count => "count",
            LeaderboardSortBy::TotalBytes => "total_bytes",
            LeaderboardSortBy::AverageSpeed => "average_speed",
        };
        let ordering = if order == SortOrder::Asc {
            "ASC"
        } else {
            "DESC"
        };
        self.database
            .call(move |db| {
                let sql = format!(
                    "SELECT username, SUM(bytes) AS total_bytes, SUM(files) AS count,
                    SUM(speed) / SUM(files) AS average_speed,
                    SUM(wait) / NULLIF(SUM(waits), 0) / 1e9,
                    SUM(duration) / NULLIF(SUM(durations), 0) / 1e9
                 FROM ({}) WHERE direction = ?7 AND state = 48
                 GROUP BY username ORDER BY {field} {ordering}, username DESC LIMIT ?8 OFFSET ?9",
                    user_rows()
                );
                let mut stmt = db.prepare_cached(&sql)?;
                let [a, b, c, d, e, f] = split.params();
                let mut rows =
                    stmt.query(rusqlite::params![a, b, c, d, e, f, dir, limit, offset])?;
                let mut out = Vec::new();
                while let Some(row) = rows.next()? {
                    out.push(TransferSummary {
                        username: row.get(0)?,
                        total_bytes: row.get(1)?,
                        count: row.get(2)?,
                        average_speed: row.get::<_, Option<f64>>(3)?.unwrap_or_default(),
                        average_wait_ms: ms(row.get(4)?),
                        average_duration_ms: ms(row.get(5)?),
                        ..Default::default()
                    });
                }
                Ok(out)
            })
            .await
    }

    pub async fn errors(
        &self,
        direction: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        username: Option<&str>,
        order: SortOrder,
        page: Page,
    ) -> anyhow::Result<Vec<TransferErrorDetail>> {
        let Page { limit, offset } = page;
        let (direction, start, end, username) = (
            direction.to_owned(),
            timestamp(start),
            timestamp(end),
            username.map(str::to_owned),
        );
        self.database.call(move |db| {
            let sql = format!("SELECT id, username, direction, filename, size, state, requested_at, enqueued_at, started_at, ended_at, bytes_transferred, average_speed, exception FROM transfer_details WHERE (state & ~48) != 0 AND direction=?1 AND ended_at IS NOT NULL AND ended_at >= ?2 AND ended_at <= ?3 AND (?4 IS NULL OR username=?4) ORDER BY ended_at {} LIMIT ?5 OFFSET ?6", if order == SortOrder::Asc { "ASC" } else { "DESC" });
            let mut stmt = db.prepare_cached(&sql)?;
            let mut rows = stmt.query(rusqlite::params![direction, start, end, username, limit, offset])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(TransferErrorDetail {
                    id: stored_id(row, 0)?, username: row.get(1)?, direction: api_direction(&row.get::<_, String>(2)?), filename: row.get(3)?,
                    size: row.get(4)?, state: summary_state(row.get(5)?),
                    requested_at: datetime(row, 6)?.ok_or(rusqlite::Error::InvalidQuery)?,
                    enqueued_at: datetime(row, 7)?, started_at: datetime(row, 8)?, ended_at: datetime(row, 9)?,
                    bytes_transferred: row.get(10)?, average_speed: row.get(11)?, error: row.get(12)?,
                });
            }
            Ok(out)
        }).await
    }

    pub async fn error_pareto(
        &self,
        direction: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
        username: Option<&str>,
        page: Page,
    ) -> anyhow::Result<Vec<TransferErrorSummary>> {
        let Page { limit, offset } = page;
        let (direction, split, username) = (
            direction.to_owned(),
            Split::new(start, end),
            username.map(str::to_owned),
        );
        self.database
            .call(move |db| {
                let mut stmt = db.prepare_cached(&format!(
                    "SELECT NULLIF(error, ''), SUM(files), COUNT(DISTINCT username) FROM ({})
                 WHERE direction = ?7 AND (?8 IS NULL OR username = ?8)
                 GROUP BY error ORDER BY SUM(files) DESC, error LIMIT ?9 OFFSET ?10",
                    windowed(
                        "SELECT direction, error, username, files FROM transfer_error_hours",
                        &format!("SELECT direction, {ERROR_TEXT}, username, 1"),
                        "transfer_details",
                        "(state & ~48) != 0",
                    )
                ))?;
                let [a, b, c, d, e, f] = split.params();
                let mut rows = stmt.query(rusqlite::params![
                    a, b, c, d, e, f, direction, username, limit, offset
                ])?;
                let mut out = Vec::new();
                while let Some(r) = rows.next()? {
                    out.push(TransferErrorSummary {
                        error: r.get(0)?,
                        count: r.get(1)?,
                        distinct_users: r.get(2)?,
                    });
                }
                Ok(out)
            })
            .await
    }

    pub async fn user_report(
        &self,
        username: &str,
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> anyhow::Result<UserTransferReport> {
        let mut result = UserTransferReport::new();
        let mut summary = self.summary(start, end, None, Some(username)).await?;
        for direction in [TransferDirection::Upload, TransferDirection::Download] {
            let by_state = summary.remove(&direction).unwrap_or_default();
            let total = by_state
                .values()
                .fold(0_u64, |sum, item| sum.saturating_add(item.count));
            let count = |state| {
                by_state
                    .get(&state)
                    .map_or(0, |item: &TransferSummary| item.count)
            };
            let successful = count(TransferState::Succeeded);
            let errored = [
                TransferState::Errored,
                TransferState::TimedOut,
                TransferState::Rejected,
                TransferState::Aborted,
            ]
            .into_iter()
            .map(count)
            .fold(0_u64, u64::saturating_add);
            let cancelled = count(TransferState::Cancelled);
            let errors = self
                .error_pareto(
                    db_direction(direction),
                    start,
                    end,
                    Some(username),
                    Page::first(25),
                )
                .await?;
            result.insert(
                direction,
                UserDirectionTransferSummary {
                    summary: by_state,
                    statistics: UserDirectionTransferStatistics {
                        total,
                        successful,
                        errored,
                        cancelled,
                    },
                    errors,
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
        page: Page,
    ) -> anyhow::Result<Vec<TransferDirectorySummary>> {
        let Page { limit, offset } = page;
        let (split, username) = (Split::new(start, end), username.map(str::to_owned));
        self.database.call(move |db| {
            let mut stmt = db.prepare_cached(&format!(
                "SELECT directory, SUM(files), COUNT(DISTINCT username) FROM ({})
                 WHERE ?7 IS NULL OR username = ?7 GROUP BY directory
                 ORDER BY COUNT(DISTINCT username) DESC, SUM(files) DESC, directory LIMIT ?8 OFFSET ?9",
                windowed(
                    "SELECT directory, username, files FROM transfer_directory_hours",
                    "SELECT directory, username, 1",
                    "transfer_details",
                    "direction = 'Upload' AND state = 48",
                )
            ))?;
            let [a, b, c, d, e, f] = split.params();
            let mut rows = stmt.query(rusqlite::params![a, b, c, d, e, f, username, limit, offset])?;
            let mut out = Vec::new();
            while let Some(r) = rows.next()? { out.push(TransferDirectorySummary { directory: r.get(0)?, count: r.get(1)?, distinct_users: r.get(2)? }); }
            Ok(out)
        }).await
    }
}

fn empty_summary() -> TransferSummaryReport {
    [TransferDirection::Download, TransferDirection::Upload]
        .into_iter()
        .map(|d| (d, TransferSummaryByState::new()))
        .collect()
}

/// Query parameters shared by the report routes; each route uses some of them.
#[derive(Default, Deserialize)]
#[serde(default)]
struct Window {
    start: Option<DateTime<Utc>>,
    end: Option<DateTime<Utc>>,
    direction: Option<TransferDirection>,
    username: Option<String>,
    limit: Option<u32>,
    offset: Option<u32>,
    sort_by: Option<LeaderboardSortBy>,
    sort_order: Option<SortOrder>,
    buckets: Option<u32>,
}

impl Window {
    fn bounds(
        &self,
        default_start: DateTime<Utc>,
        default_end: DateTime<Utc>,
    ) -> ApiResult<(DateTime<Utc>, DateTime<Utc>)> {
        let start = self.start.unwrap_or(default_start);
        let end = self.end.unwrap_or(default_end);
        if start >= end {
            return Err(bad_request("end must be later than start"));
        }
        Ok((start, end))
    }

    fn recent_bounds(&self) -> ApiResult<(DateTime<Utc>, DateTime<Utc>)> {
        let now = Utc::now();
        self.bounds(now - Duration::days(7), now)
    }

    fn direction(&self) -> Option<&'static str> {
        self.direction.map(db_direction)
    }

    fn required_direction(&self) -> ApiResult<&'static str> {
        self.direction()
            .ok_or_else(|| bad_request("direction is required (download or upload)"))
    }

    fn username(&self) -> Option<&str> {
        self.username
            .as_deref()
            .filter(|value| !value.trim().is_empty())
    }

    fn paging(&self) -> ApiResult<Page> {
        let limit = self.limit.unwrap_or(25);
        if limit == 0 {
            return Err(bad_request("limit must be greater than zero"));
        }
        Ok(Page {
            limit: limit.into(),
            offset: self.offset.unwrap_or(0).into(),
        })
    }

    /// Bucket width in seconds.
    fn histogram_seconds(&self, start: DateTime<Utc>, end: DateTime<Utc>) -> ApiResult<i64> {
        let buckets = self.buckets.unwrap_or(100);
        if !(1..=1000).contains(&buckets) {
            return Err(bad_request("buckets must be between 1 and 1000"));
        }
        Ok((end.signed_duration_since(start).num_seconds() / i64::from(buckets)).max(1))
    }
}

fn bad_request(message: &str) -> ApiError {
    ApiError::BadRequest(message.to_owned())
}

async fn summary(
    _: Authenticated,
    State(state): State<AppState>,
    Query(q): Query<Window>,
) -> ApiResult<Json<TransferSummaryReport>> {
    let (start, end) = q.recent_bounds()?;
    Ok(Json(
        state
            .services
            .telemetry
            .reports
            .summary(start, end, q.direction(), q.username())
            .await?,
    ))
}

async fn histogram(
    _: Authenticated,
    State(state): State<AppState>,
    Query(q): Query<Window>,
) -> ApiResult<Json<TransferHistogram>> {
    let (mut start, end) = q.recent_bounds()?;
    if end < GENESIS {
        return Err(bad_request("end is earlier than 2020-12-30 06:22 UTC"));
    }
    let reports = &state.services.telemetry.reports;
    if start < GENESIS {
        start = reports.oldest_requested().await?.unwrap_or(GENESIS);
    }
    let seconds = q.histogram_seconds(start, end)?;
    if histogram_bucket_count(start, end, seconds) > MAX_HISTOGRAM_BUCKETS {
        return Err(bad_request(&format!(
            "Histogram would exceed {MAX_HISTOGRAM_BUCKETS} buckets; use fewer buckets or a shorter window"
        )));
    }
    Ok(Json(
        reports
            .histogram(start, end, seconds, q.direction(), q.username())
            .await?,
    ))
}

async fn chart(
    _: Authenticated,
    State(state): State<AppState>,
    Query(q): Query<Window>,
) -> ApiResult<Json<TransferHistogram>> {
    let (mut start, end) = q.recent_bounds()?;
    if end < GENESIS {
        return Err(bad_request("end is earlier than 2020-12-30 06:22 UTC"));
    }
    let reports = &state.services.telemetry.reports;
    if start < GENESIS {
        start = reports
            .database
            .call(|db| {
                db.query_row("SELECT MIN(hour) FROM transfer_chart_hours", [], |row| {
                    row.get::<_, Option<i64>>(0)
                })
            })
            .await?
            .and_then(|hour| DateTime::from_timestamp(hour, 0))
            .unwrap_or(GENESIS)
            .min(end - Duration::hours(1));
    }
    let seconds = q.histogram_seconds(start, end)?;
    let hours = (seconds + 3599) / 3600;
    Ok(Json(reports.chart(start, end, hours * 3600).await?))
}

async fn leaderboard(
    _: Authenticated,
    State(state): State<AppState>,
    Query(q): Query<Window>,
) -> ApiResult<Json<TransferLeaderboard>> {
    let direction = q.required_direction()?;
    let (start, end) = q.bounds(GENESIS, Utc::now())?;
    let page = q.paging()?;
    Ok(Json(
        state
            .services
            .telemetry
            .reports
            .leaderboard(
                direction,
                start,
                end,
                q.sort_by.unwrap_or_default(),
                q.sort_order.unwrap_or_default(),
                page,
            )
            .await?,
    ))
}

async fn errors(
    _: Authenticated,
    State(state): State<AppState>,
    Query(q): Query<Window>,
) -> ApiResult<Json<Vec<TransferErrorDetail>>> {
    let direction = q.required_direction()?;
    let (start, end) = q.bounds(MIN_TIME, MAX_TIME)?;
    let page = q.paging()?;
    Ok(Json(
        state
            .services
            .telemetry
            .reports
            .errors(
                direction,
                start,
                end,
                q.username(),
                q.sort_order.unwrap_or_default(),
                page,
            )
            .await?,
    ))
}

async fn pareto(
    _: Authenticated,
    State(state): State<AppState>,
    Query(q): Query<Window>,
) -> ApiResult<Json<Vec<TransferErrorSummary>>> {
    let direction = q.required_direction()?;
    let (start, end) = q.bounds(MIN_TIME, MAX_TIME)?;
    let page = q.paging()?;
    Ok(Json(
        state
            .services
            .telemetry
            .reports
            .error_pareto(direction, start, end, q.username(), page)
            .await?,
    ))
}

async fn user_report(
    _: Authenticated,
    State(state): State<AppState>,
    Path(username): Path<String>,
    Query(q): Query<Window>,
) -> ApiResult<Json<UserTransferReport>> {
    if username.trim().is_empty() {
        return Err(bad_request("username is required"));
    }
    let (start, end) = q.bounds(GENESIS, Utc::now())?;
    Ok(Json(
        state
            .services
            .telemetry
            .reports
            .user_report(&username, start, end)
            .await?,
    ))
}

async fn directories(
    _: Authenticated,
    State(state): State<AppState>,
    Query(q): Query<Window>,
) -> ApiResult<Json<Vec<TransferDirectorySummary>>> {
    let (start, end) = q.bounds(GENESIS, MAX_TIME)?;
    let page = q.paging()?;
    Ok(Json(
        state
            .services
            .telemetry
            .reports
            .directories(start, end, q.username(), page)
            .await?,
    ))
}

pub fn routes() -> Routes {
    Routes::new()
        .get("/api/reports/transfers/summary", summary)
        .get("/api/reports/transfers/histogram", histogram)
        .get("/api/reports/transfers/chart", chart)
        .get("/api/reports/transfers/leaderboard", leaderboard)
        .get("/api/reports/transfers/errors", errors)
        .get("/api/reports/transfers/errors/pareto", pareto)
        .get("/api/reports/transfers/directories", directories)
        .get("/api/reports/transfers/users/{username}", user_report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: DateTime<Utc> = utc(1_767_225_600, 0);
    const END: DateTime<Utc> = utc(1_767_225_720, 250_000_000);

    #[test]
    fn queries_use_snake_case_names_and_values() {
        let uri = "/?direction=download&sort_by=total_bytes&sort_order=asc&buckets=2"
            .parse()
            .unwrap();
        let axum::extract::Query(query) =
            axum::extract::Query::<Window>::try_from_uri(&uri).unwrap();
        assert_eq!(query.required_direction().unwrap(), "Download");
        assert_eq!(query.sort_by, Some(LeaderboardSortBy::TotalBytes));
        assert_eq!(query.sort_order, Some(SortOrder::Asc));
        assert_eq!(query.histogram_seconds(START, END).unwrap(), 60);

        for invalid in [
            "/?direction=Download",
            "/?sort_order=ASC",
            "/?sortBy=count&limit=x",
        ] {
            let uri = invalid.parse().unwrap();
            assert!(
                axum::extract::Query::<Window>::try_from_uri(&uri).is_err(),
                "{invalid}"
            );
        }
        let query = Window {
            buckets: Some(5),
            ..Window::default()
        };
        assert_eq!(query.histogram_seconds(START, END).unwrap(), 24);
        let query = Window {
            buckets: Some(1001),
            ..Window::default()
        };
        assert!(query.histogram_seconds(START, END).is_err());
    }

    #[tokio::test]
    async fn histogram_rejects_unbounded_bucket_counts() {
        assert_eq!(histogram_bucket_count(START, END, 120), 2);
        assert_eq!(
            histogram_bucket_count(START, START + Duration::minutes(4), 120),
            2
        );
        let end = utc(253_402_214_400, 0);
        assert!(histogram_bucket_count(GENESIS, end, 120) > MAX_HISTOGRAM_BUCKETS);
        assert!(
            fixture()
                .0
                .histogram(GENESIS, end, 120, None, None)
                .await
                .is_err()
        );
        // Bucket counts stay within the cap even across the widest window.
        let query = Window {
            buckets: Some(1000),
            ..Window::default()
        };
        let seconds = query.histogram_seconds(MIN_TIME, MAX_TIME).unwrap();
        assert!(histogram_bucket_count(MIN_TIME, MAX_TIME, seconds) <= MAX_HISTOGRAM_BUCKETS);
    }

    /// Report data written by the text-timestamp schema, then converted by the
    /// integer-times migration, so every report test also covers that migration.
    fn fixture() -> (ReportsService, tempfile::TempDir) {
        use crate::transfers::TRANSFER_MIGRATIONS as MIGRATIONS;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("transfers.db");
        let integer_times = MIGRATIONS
            .iter()
            .position(|sql| *sql == include_str!("../transfers/migrations/integer_times.sql"))
            .unwrap();
        drop(Database::open(&path, &MIGRATIONS[..integer_times]).unwrap());
        rusqlite::Connection::open(&path).unwrap().execute_batch(r#"
            INSERT INTO transfers (id, username, direction, filename, size, state, requested_at,
                enqueued_at, started_at, ended_at, bytes_transferred, average_speed, exception) VALUES
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
        "#).unwrap();
        let database = Database::open(&path, MIGRATIONS).unwrap();
        (ReportsService::new(database), directory)
    }

    /// The stored form of an RFC 3339 time.
    fn stored(time: &str) -> i64 {
        timestamp(time.parse().unwrap())
    }

    /// SQL storing `filename` in the interned path tables, for [`file`].
    fn intern_file(filename: &str) -> String {
        let split = filename.rfind(['/', '\\']).map_or(0, |i| i + 1);
        let (path, name) = filename.split_at(split);
        format!(
            "INSERT INTO transfer_directories (path) VALUES ('{path}') ON CONFLICT DO NOTHING;
             INSERT INTO transfer_files (directory_id, name)
             SELECT directory_id, '{name}' FROM transfer_directories WHERE path = '{path}'
             ON CONFLICT DO NOTHING;"
        )
    }

    /// A SQL expression for the `file_id` of `filename` (stored by [`intern_file`]).
    fn file(filename: &str) -> String {
        format!(
            "(SELECT file_id FROM transfer_files JOIN transfer_directories USING (directory_id)
              WHERE path || name = '{filename}')"
        )
    }

    /// SQL storing `error`, then a SQL expression for its `error_id`.
    fn error(error: &str) -> (String, String) {
        (
            format!(
                "INSERT INTO transfer_errors (exception) VALUES ('{error}') ON CONFLICT DO NOTHING;"
            ),
            format!("(SELECT error_id FROM transfer_errors WHERE exception = '{error}')"),
        )
    }

    #[tokio::test]
    async fn chart_keeps_exact_edges_and_tracks_transfer_changes() {
        let (reports, _directory) = fixture();
        let start = START - Duration::minutes(5);
        let end = START + Duration::hours(2) + Duration::minutes(2);
        let chart = |histogram: &TransferHistogram, hour, direction, state| {
            let summary = &histogram[&hour][&direction][&state];
            (summary.count, summary.total_bytes)
        };
        let first = reports.chart(start, END, 3600).await.unwrap();
        assert_eq!(
            chart(
                &first,
                START,
                TransferDirection::Download,
                TransferState::Succeeded
            ),
            (2, 720)
        );
        let first = reports.chart(start, end, 3600).await.unwrap();
        assert_eq!(
            chart(
                &first,
                START,
                TransferDirection::Download,
                TransferState::Succeeded
            ),
            (3, 1620)
        );

        let changes = format!(
            "DELETE FROM transfers WHERE id = 'd2';
             UPDATE transfers SET direction = 'Upload', size = 50 WHERE id = 'd1';
             UPDATE transfers SET state = 48, ended_at = {} WHERE id = 'd3';
             {}
             INSERT INTO transfers (id, username, direction, file_id, size, state, requested_at, ended_at)
             VALUES ('d5', 'alice', 'Download', {}, 7, 48, 0, {});",
            stored("2026-01-01T02:01:00.000000000Z"),
            intern_file("five.flac"),
            file("five.flac"),
            stored("2026-01-01T01:59:59.999999999Z"),
        );
        reports
            .database
            .call(move |db| db.execute_batch(&changes))
            .await
            .unwrap();
        let updated = reports.chart(start, end, 3600).await.unwrap();
        assert_eq!(
            chart(
                &updated,
                START,
                TransferDirection::Download,
                TransferState::Succeeded
            ),
            (1, 900)
        );
        assert_eq!(
            chart(
                &updated,
                START,
                TransferDirection::Upload,
                TransferState::Succeeded
            ),
            (3, 160)
        );
        assert_eq!(
            chart(
                &updated,
                START + Duration::hours(1),
                TransferDirection::Download,
                TransferState::Succeeded
            ),
            (1, 7)
        );
        assert_eq!(
            chart(
                &updated,
                START + Duration::hours(2),
                TransferDirection::Download,
                TransferState::Succeeded
            ),
            (1, 90)
        );
    }

    #[tokio::test]
    async fn summary_and_histogram_match_direction_state_and_window() {
        let (reports, _directory) = fixture();
        let summary = reports
            .summary(START, END, Some("Download"), None)
            .await
            .unwrap();
        let succeeded = &summary[&TransferDirection::Download][&TransferState::Succeeded];
        assert_eq!(
            (
                succeeded.count,
                succeeded.total_bytes,
                succeeded.distinct_users
            ),
            (2, 720, 2)
        );
        assert_eq!(succeeded.average_speed, 9.0);
        assert_eq!(succeeded.average_wait_ms, 1000);
        assert_eq!(
            summary[&TransferDirection::Download][&TransferState::Errored].count,
            1
        );
        assert!(summary[&TransferDirection::Upload].is_empty());

        let histogram = reports
            .histogram(START, END, 120, Some("Download"), None)
            .await
            .unwrap();
        assert_eq!(histogram.len(), 2);
        let final_bucket = &histogram[&(START + Duration::minutes(2))];
        // d2 alone: 600 bytes over the exact 2.25 s (whole seconds once made it 2 s).
        assert_eq!(
            final_bucket[&TransferDirection::Download][&TransferState::Succeeded].average_speed,
            600.0 / 2.25
        );
        assert!(final_bucket[&TransferDirection::Upload].is_empty());
    }

    #[tokio::test]
    async fn dashboard_lists_and_user_details_use_transfer_history() {
        let (reports, _directory) = fixture();
        let top = reports
            .leaderboard(
                "Download",
                START,
                END,
                LeaderboardSortBy::TotalBytes,
                SortOrder::Desc,
                Page::first(1),
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
                SortOrder::Desc,
                Page::first(1),
            )
            .await
            .unwrap();
        assert_eq!(top[0].username.as_deref(), Some("alice"));

        let details = reports
            .errors(
                "Download",
                MIN_TIME,
                MAX_TIME,
                None,
                SortOrder::Desc,
                Page::first(25),
            )
            .await
            .unwrap();
        assert_eq!(details.len(), 1);
        assert_eq!(details[0].state, TransferState::Errored);
        assert_eq!(details[0].error.as_deref(), Some("refused"));
        assert_eq!(details[0].requested_at, START);
        let user = reports.user_report("alice", START, END).await.unwrap();
        assert_eq!(user[&TransferDirection::Download].statistics.total, 2);
        assert_eq!(user[&TransferDirection::Download].statistics.successful, 1);
        assert_eq!(user[&TransferDirection::Download].statistics.errored, 1);
        assert_eq!(
            user[&TransferDirection::Download].errors[0]
                .error
                .as_deref(),
            Some("refused")
        );
        assert_eq!(user[&TransferDirection::Upload].statistics.successful, 1);

        let dirs = reports
            .directories(START, END, None, Page::first(25))
            .await
            .unwrap();
        assert_eq!(dirs.len(), 1);
        assert_eq!(dirs[0].directory.as_deref(), Some(r"music\album"));
        assert_eq!((dirs[0].count, dirs[0].distinct_users), (2, 2));
    }

    /// Each rollup table, sorted, and what the migration's backfill makes of `transfers`.
    fn rollups(db: &rusqlite::Connection) -> Vec<(Vec<String>, Vec<String>)> {
        let backfill = include_str!("../transfers/migrations/compact.sql");
        let dump = |sql: &str| -> Vec<String> {
            let mut stmt = db.prepare(sql).unwrap();
            let columns = stmt.column_count();
            let mut rows: Vec<String> = stmt
                .query_map([], |row| {
                    (0..columns)
                        .map(|i| {
                            row.get::<_, rusqlite::types::Value>(i)
                                .map(|v| format!("{v:?}"))
                        })
                        .collect::<rusqlite::Result<Vec<_>>>()
                        .map(|values| values.join("|"))
                })
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            rows.sort();
            rows
        };
        [
            "transfer_user_hours",
            "transfer_error_hours",
            "transfer_directory_hours",
        ]
        .into_iter()
        .map(|table| {
            let marker = format!("INSERT INTO {table}\nSELECT");
            let start = backfill.find(&marker).unwrap() + marker.len() - "SELECT".len();
            let end = start + backfill[start..].find(';').unwrap();
            (
                dump(&format!("SELECT * FROM {table}")),
                dump(&backfill[start..end]),
            )
        })
        .collect()
    }

    #[tokio::test]
    async fn report_hours_follow_transfer_changes() {
        let (reports, _directory) = fixture();
        let hour = |h: i64| stored("2026-01-01T00:00:00Z") + h * 3_600_000_000_000;
        let files = [
            r"music\other\a.flac",
            r"music\other\b.flac",
            "music/other/c.flac",
            r"music\moved\two.flac",
        ];
        let (timed_out, timed_out_id) = error("timed out");
        let (gone, gone_id) = error("gone");
        let changes = format!(
            "{interned} {timed_out} {gone}
             INSERT INTO transfers (id, username, direction, file_id, size, state, requested_at,
                 enqueued_at, started_at, ended_at, average_speed, error_id) VALUES
                 ('u3', 'carol', 'Upload', {f0}, 5, 48, 0, {e}, {s}, {a}, 3, NULL),
                 ('u4', 'carol', 'Upload', {f1}, 6, 272, 0, NULL, NULL, {b}, 0, {timed_out_id}),
                 ('u5', 'dave', 'Upload', {f2}, 7, 2, 0, NULL, NULL, NULL, 0, NULL);
             UPDATE transfers SET state = 48, ended_at = {b}, started_at = {b} - 3000000000, average_speed = 2 WHERE id = 'u5';
             UPDATE transfers SET state = 1040, error_id = {gone_id} WHERE id = 'u1';
             UPDATE transfers SET username = 'erin', file_id = {f3} WHERE id = 'u2';
             UPDATE transfers SET ended_at = NULL, state = 2 WHERE id = 'd3';
             DELETE FROM transfers WHERE id IN ('d2', 'u4');",
            interned = files.map(intern_file).concat(),
            f0 = file(files[0]),
            f1 = file(files[1]),
            f2 = file(files[2]),
            f3 = file(files[3]),
            e = hour(1),
            s = hour(1) + 2_000_000_000,
            a = hour(1) + 5_000_000_000,
            b = hour(2) + 1,
        );
        let tables = reports
            .database
            .call(move |db| {
                let before = rollups(db);
                db.execute_batch(&changes)?;
                Ok((before, rollups(db)))
            })
            .await
            .unwrap();
        for (kept, recomputed) in [tables.0, tables.1].into_iter().flatten() {
            assert!(!recomputed.is_empty());
            assert_eq!(kept, recomputed);
        }

        // Whole hours come from the rollups and the edges from the transfers.
        let start = START - Duration::minutes(30);
        let end = START + Duration::hours(2) + Duration::minutes(30);
        let summary = reports
            .summary(start, end, Some("Upload"), None)
            .await
            .unwrap();
        let succeeded = &summary[&TransferDirection::Upload][&TransferState::Succeeded];
        assert_eq!(
            (
                succeeded.count,
                succeeded.total_bytes,
                succeeded.distinct_users
            ),
            (3, 82, 3)
        );
        assert_eq!(succeeded.average_speed, 4.0);
        assert_eq!(succeeded.average_wait_ms, 2000);
        assert_eq!(succeeded.average_duration_ms, 3000);
        let top = reports
            .leaderboard(
                "Upload",
                start,
                end,
                LeaderboardSortBy::TotalBytes,
                SortOrder::Desc,
                Page::first(1),
            )
            .await
            .unwrap();
        assert_eq!(
            (top[0].username.as_deref(), top[0].total_bytes),
            (Some("erin"), 70)
        );
        let errors = reports
            .error_pareto("Upload", start, end, None, Page::first(25))
            .await
            .unwrap();
        assert_eq!(errors.len(), 1);
        assert_eq!(
            (errors[0].error.as_deref(), errors[0].count),
            (Some("gone"), 1)
        );
        let dirs = reports
            .directories(start, end, None, Page::first(25))
            .await
            .unwrap();
        let dirs: Vec<_> = dirs
            .iter()
            .map(|d| (d.directory.as_deref().unwrap(), d.count))
            .collect();
        assert_eq!(
            dirs,
            [("music/other", 1), (r"music\moved", 1), (r"music\other", 1)]
        );
    }
}
