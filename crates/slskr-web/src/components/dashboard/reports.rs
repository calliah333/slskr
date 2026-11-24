use leptos::prelude::*;
use slskr_types::telemetry::{
    TransferDirectorySummary, TransferErrorDetail, TransferErrorSummary, TransferHistogram,
    TransferLeaderboard, TransferSummaryReport,
};

use crate::services::{api, util::encode_uri_component};
use serde::de::DeserializeOwned;
use std::{collections::HashMap, marker::PhantomData};

fn query(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(key, value)| format!("{key}={}", encode_uri_component(value)))
        .collect::<Vec<_>>()
        .join("&")
}

pub fn summary(start: &str, end: &str) -> ReportRequest<TransferSummaryReport> {
    ReportRequest::new("summary", start, end, &[])
}

pub fn histogram(start: &str, end: &str, buckets: usize) -> ReportRequest<TransferHistogram> {
    ReportRequest::new("chart", start, end, &[("buckets", buckets.to_string())])
}

pub fn leaderboard(
    start: &str,
    end: &str,
    direction: &str,
    sort: &str,
) -> ReportRequest<TransferLeaderboard> {
    ReportRequest::new(
        "leaderboard",
        start,
        end,
        &[
            ("direction", direction.to_owned()),
            ("sort_by", sort.to_owned()),
            ("sort_order", "desc".into()),
            ("limit", "10".into()),
        ],
    )
}

pub fn directories(start: &str, end: &str) -> ReportRequest<Vec<TransferDirectorySummary>> {
    ReportRequest::new("directories", start, end, &[("limit", "10".into())])
}

pub fn error_types(
    start: &str,
    end: &str,
    direction: &str,
) -> ReportRequest<Vec<TransferErrorSummary>> {
    ReportRequest::new(
        "errors/pareto",
        start,
        end,
        &[("direction", direction.to_owned()), ("limit", "10".into())],
    )
}

pub fn errors(start: &str, end: &str, direction: &str) -> ReportRequest<Vec<TransferErrorDetail>> {
    ReportRequest::new(
        "errors",
        start,
        end,
        &[
            ("direction", direction.to_owned()),
            ("sort_order", "desc".into()),
            ("limit", "10".into()),
        ],
    )
}

pub(super) struct ReportRequest<T> {
    path: String,
    kind: String,
    end: String,
    report: PhantomData<T>,
}

struct CachedReport {
    end: String,
    expires: f64,
    body: String,
}

/// A snapshot per report and period for the lifetime of the signed-in app.
#[derive(Clone, Copy)]
pub(crate) struct ReportCache(StoredValue<HashMap<(usize, String), CachedReport>>);

impl ReportCache {
    pub(crate) fn new() -> Self {
        Self(StoredValue::new(HashMap::new()))
    }
}

impl<T: DeserializeOwned> ReportRequest<T> {
    fn new(report: &str, start: &str, end: &str, params: &[(&str, String)]) -> Self {
        let extra = query(params);
        let kind = if extra.is_empty() {
            report.to_owned()
        } else {
            format!("{report}?{extra}")
        };
        let mut query = query(&[("start", start.into()), ("end", end.into())]);
        if !extra.is_empty() {
            query.push('&');
            query.push_str(&extra);
        }
        Self {
            path: format!("/reports/transfers/{report}?{query}"),
            kind,
            end: end.to_owned(),
            report: PhantomData,
        }
    }

    pub fn cached(&self, cache: ReportCache, period: usize) -> Option<(T, bool)> {
        cache
            .0
            .try_with_value(|cache| {
                let cached = cache.get(&(period, self.kind.clone()))?;
                let fresh = cached.end == self.end && cached.expires > js_sys::Date::now();
                Some((serde_json::from_str(&cached.body).ok()?, fresh))
            })
            .flatten()
    }

    pub async fn fetch(self, cache: ReportCache, period: usize) -> Result<T, api::ApiError> {
        let body = api::get_text(&self.path).await?;
        let result = serde_json::from_str(&body).map_err(|error| api::ApiError {
            status: 0,
            message: format!("Invalid response: {error}"),
            retry_after: None,
        })?;
        cache.0.try_update_value(|cache| {
            let entry = cache.entry((period, self.kind));
            // A slow request for an older window must not replace a newer snapshot.
            if matches!(&entry, std::collections::hash_map::Entry::Occupied(entry) if entry.get().end > self.end) {
                return;
            }
            entry.insert_entry(CachedReport { end: self.end, expires: js_sys::Date::now() + 60_000.0, body });
        });
        Ok(result)
    }
}
