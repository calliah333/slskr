//! Live transfer telemetry and report endpoints.

pub mod reports;

use std::{fmt::Write as _, time::Duration};

use crate::{
    app::{AppState, Core},
    auth::{Authenticated, constant_time_eq},
    http::{Json, Routes},
    transfers::TransferService,
};
use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use slskr_types::{
    config::MetricsConfig,
    realtime::{Event, Topic},
    telemetry::CurrentMetrics,
};

#[derive(Clone)]
pub struct TelemetryService {
    pub reports: reports::ReportsService,
    transfers: TransferService,
    bus: crate::realtime::EventBus,
    scrape_token: Option<String>,
}

impl TelemetryService {
    pub fn new(core: &Core, transfers: &TransferService) -> anyhow::Result<Self> {
        let config = &core.options.startup().metrics;
        Ok(Self {
            reports: reports::ReportsService::new(transfers.database().reader()?),
            transfers: transfers.clone(),
            bus: core.bus.clone(),
            scrape_token: if config.enabled {
                configured_token(config)
            } else {
                None
            },
        })
    }

    /// Current live transfer counters supplied by the transfer service.
    pub fn metrics(&self) -> CurrentMetrics {
        self.transfers.metrics()
    }

    /// Publishes the current metrics every second while the `metrics` topic has subscribers
    /// (a new subscriber gets them at once as its snapshot).
    pub fn start(&self, core: &Core) {
        let transfers = self.transfers.clone();
        let bus = self.bus.clone();
        let shutdown = core.shutdown.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(1));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = ticker.tick() => {
                        bus.publish_with(Topic::Metrics, || Event::Metrics(transfers.metrics()));
                    }
                }
            }
        });
    }
}

struct Metric {
    name: &'static str,
    help: &'static str,
    kind: &'static str,
    value: f64,
}

fn transfer_metrics(current: &CurrentMetrics) -> impl Iterator<Item = Metric> {
    macro_rules! families {
        ($(($kind:literal, $suffix:literal, $before:literal, $after:literal, $group:ident.$field:ident)),+ $(,)?) => {{
            [
                $(Metric {
                    name: concat!("slskr_transfers_downloads_", $suffix),
                    help: concat!($before, "downloads", $after),
                    kind: $kind,
                    value: current.downloads.$group.$field as f64,
                },)+
                $(Metric {
                    name: concat!("slskr_transfers_uploads_", $suffix),
                    help: concat!($before, "uploads", $after),
                    kind: $kind,
                    value: current.uploads.$group.$field as f64,
                },)+
            ].into_iter()
        }};
    }
    families![
        (
            "gauge",
            "in_progress_files_current",
            "Current number of in-progress ",
            "",
            in_progress.files
        ),
        (
            "gauge",
            "in_progress_users_current",
            "Current number of unique users with in-progress ",
            "",
            in_progress.users
        ),
        (
            "gauge",
            "in_progress_speed_average_current",
            "Current average speed, in bytes per second, of in-progress ",
            "",
            in_progress.average_speed
        ),
        (
            "gauge",
            "in_progress_speed_total_current",
            "Current total speed, in bytes per second, of in-progress ",
            "",
            in_progress.total_speed
        ),
        (
            "gauge",
            "queued_files_current",
            "Current number of queued ",
            "",
            queued.files
        ),
        (
            "gauge",
            "queued_users_current",
            "Current number of unique users with queued ",
            "",
            queued.users
        ),
        (
            "gauge",
            "queued_bytes_current",
            "Current total size, in bytes, of queued ",
            "",
            queued.bytes
        ),
        (
            "counter",
            "completed_succeeded",
            "Total number of ",
            " that completed successfully",
            completed.succeeded
        ),
        (
            "counter",
            "completed_failed",
            "Total number of ",
            " that failed",
            completed.failed
        ),
        (
            "counter",
            "completed_bytes",
            "Total number of bytes transferred by successfully completed ",
            "",
            completed.bytes
        ),
    ]
}

fn render_metrics(current: &CurrentMetrics) -> String {
    let mut text = String::new();
    for Metric {
        name,
        help,
        kind,
        value,
    } in transfer_metrics(current)
    {
        let _ = writeln!(text, "# HELP {name} {help}\n# TYPE {name} {kind}");
        if value.is_infinite() {
            let sign = if value.is_sign_negative() { '-' } else { '+' };
            let _ = writeln!(text, "{name} {sign}Inf");
        } else {
            let _ = writeln!(text, "{name} {value}");
        }
    }
    text
}

async fn current_metrics(_: Authenticated, State(state): State<AppState>) -> Json<CurrentMetrics> {
    Json(state.services.telemetry.metrics())
}

/// Basic credentials, or `None` (no authentication) when neither is configured.
fn configured_token(config: &MetricsConfig) -> Option<String> {
    (!config.username.is_empty() || !config.password.is_empty())
        .then(|| STANDARD.encode(format!("{}:{}", config.username, config.password)))
}

fn scrape_authorized(headers: &HeaderMap, token: Option<&str>) -> bool {
    let Some(token) = token else { return true };
    let Some((scheme, supplied)) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split_once(' '))
    else {
        return false;
    };
    scheme.eq_ignore_ascii_case("Basic")
        && constant_time_eq(
            token.as_bytes(),
            supplied.trim_start_matches(' ').as_bytes(),
        )
}

async fn scrape(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !scrape_authorized(&headers, state.services.telemetry.scrape_token.as_deref()) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"metrics\"")],
        )
            .into_response();
    }
    (
        [(
            header::CONTENT_TYPE,
            "text/plain; version=0.0.4; charset=utf-8",
        )],
        {
            let mut text = render_metrics(&state.services.telemetry.metrics());
            crate::profiling::render(&mut text);
            text
        },
    )
        .into_response()
}

/// The separate scrape endpoint uses its own configured Basic credentials.
pub fn prometheus_routes(path: &str) -> Router<AppState> {
    Router::new().route(path, get(scrape))
}

pub fn routes() -> Routes {
    reports::routes().get("/api/metrics", current_metrics)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposition_reports_transfer_samples() {
        let mut current = CurrentMetrics::default();
        current.downloads.in_progress.total_speed = 2_048.5;
        current.uploads.completed.succeeded = 7;
        let text = render_metrics(&current);
        let speed = "slskr_transfers_downloads_in_progress_speed_total_current";
        let succeeded = "slskr_transfers_uploads_completed_succeeded";
        assert!(text.contains(&format!("# TYPE {speed} gauge\n{speed} 2048.5\n")));
        assert!(text.contains(&format!("# TYPE {succeeded} counter\n{succeeded} 7\n")));
    }

    #[test]
    fn scrape_requires_configured_basic_credentials_unless_unset() {
        let mut auth = MetricsConfig {
            username: "scraper".into(),
            password: "a:b".into(),
            ..Default::default()
        };
        let token = configured_token(&auth);
        let mut headers = HeaderMap::new();
        assert!(!scrape_authorized(&headers, token.as_deref()));
        for (scheme, credentials, allowed) in [
            ("bAsIc", "scraper:a:b", true),
            ("Basic", "scraper:a:c", false),
            ("Basic", "scrapeX:a:b", false),
            ("Bearer", "scraper:a:b", false),
        ] {
            let value = format!("{scheme} {}", STANDARD.encode(credentials));
            headers.insert(header::AUTHORIZATION, value.parse().unwrap());
            assert_eq!(scrape_authorized(&headers, token.as_deref()), allowed);
        }
        headers.insert(header::AUTHORIZATION, "Basic !?".parse().unwrap());
        assert!(!scrape_authorized(&headers, token.as_deref()));
        auth.username.clear();
        auth.password.clear();
        headers.clear();
        assert!(scrape_authorized(
            &headers,
            configured_token(&auth).as_deref()
        ));
    }
}
