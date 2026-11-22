use slskr_api::telemetry::{
    TransferDirectorySummary, TransferExceptionDetail, TransferExceptionSummary, TransferHistogram,
    TransferLeaderboard, TransferSummaryReport,
};

use crate::services::{api, util::encode_uri_component};

fn query(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{key}={}", encode_uri_component(value)))
        .collect::<Vec<_>>()
        .join("&")
}

fn common(start: &str, end: &str) -> Vec<(&'static str, String)> {
    vec![("start", start.to_owned()), ("end", end.to_owned())]
}

pub async fn summary(start: &str, end: &str) -> Result<TransferSummaryReport, api::ApiError> {
    api::get(&format!(
        "/telemetry/reports/transfers/summary?{}",
        query(&common(start, end))
    ))
    .await
}

pub async fn histogram(
    start: &str,
    end: &str,
    buckets: usize,
) -> Result<TransferHistogram, api::ApiError> {
    let mut params = common(start, end);
    params.push(("buckets", buckets.to_string()));
    api::get(&format!(
        "/telemetry/reports/transfers/histogram?{}",
        query(&params)
    ))
    .await
}

pub async fn leaderboard(
    start: &str,
    end: &str,
    direction: &str,
    sort: &str,
) -> Result<TransferLeaderboard, api::ApiError> {
    let mut params = common(start, end);
    params.extend([
        ("direction", direction.to_owned()),
        ("sortBy", sort.to_owned()),
        ("sortOrder", "DESC".to_owned()),
        ("limit", "10".to_owned()),
        ("offset", "0".to_owned()),
    ]);
    api::get(&format!(
        "/telemetry/reports/transfers/leaderboard?{}",
        query(&params)
    ))
    .await
}

pub async fn directories(
    start: &str,
    end: &str,
) -> Result<Vec<TransferDirectorySummary>, api::ApiError> {
    let mut params = common(start, end);
    params.extend([("limit", "10".into()), ("offset", "0".into())]);
    api::get(&format!(
        "/telemetry/reports/transfers/directories?{}",
        query(&params)
    ))
    .await
}

pub async fn error_types(
    start: &str,
    end: &str,
    direction: &str,
) -> Result<Vec<TransferExceptionSummary>, api::ApiError> {
    let mut params = common(start, end);
    params.extend([
        ("direction", direction.to_owned()),
        ("limit", "10".into()),
        ("offset", "0".into()),
    ]);
    api::get(&format!(
        "/telemetry/reports/transfers/exceptions/pareto?{}",
        query(&params)
    ))
    .await
}

pub async fn errors(
    start: &str,
    end: &str,
    direction: &str,
) -> Result<Vec<TransferExceptionDetail>, api::ApiError> {
    let mut params = common(start, end);
    params.extend([
        ("direction", direction.to_owned()),
        ("sortOrder", "DESC".into()),
        ("limit", "10".into()),
        ("offset", "0".into()),
    ]);
    api::get(&format!(
        "/telemetry/reports/transfers/exceptions?{}",
        query(&params)
    ))
    .await
}
