mod charts;
mod reports;
pub(crate) use reports::ReportCache;

use std::collections::{HashSet, VecDeque};

use chrono::{DateTime, Duration, Local, Utc};
use leptos::prelude::*;
use leptos_router::components::A;
use slskr_types::{
    telemetry::{
        CurrentMetrics, TransferDirectorySummary, TransferErrorDetail, TransferErrorSummary,
        TransferHistogram, TransferLeaderboard, TransferSummaryReport,
    },
    transfers::{TransferDirection, TransferState},
};
use wasm_bindgen_futures::spawn_local;

use crate::{
    components::app::AppContext,
    config,
    services::util,
    ui::{Alert, Badge, Card, Icon, PageHeader, TabItem, Tabs},
};
use charts::{HistoryChart, Sparkline};

#[derive(Clone, Copy)]
struct Period {
    label: &'static str,
    days: Option<i64>,
    buckets: usize,
}
const PERIODS: [Period; 7] = [
    Period {
        label: "24h",
        days: Some(1),
        buckets: 24,
    },
    Period {
        label: "7d",
        days: Some(7),
        buckets: 84,
    },
    Period {
        label: "30d",
        days: Some(30),
        buckets: 60,
    },
    Period {
        label: "90d",
        days: Some(90),
        buckets: 90,
    },
    Period {
        label: "180d",
        days: Some(180),
        buckets: 90,
    },
    Period {
        label: "1y",
        days: Some(365),
        buckets: 100,
    },
    Period {
        label: "All",
        days: None,
        buckets: 100,
    },
];

/// One report: `None` while it loads, then its result.
type Slot<T> = RwSignal<Option<Result<T, String>>>;

/// The history reports, each filled independently as its request answers.
#[derive(Clone, Copy)]
struct Reports {
    summary: Slot<TransferSummaryReport>,
    histogram: Slot<TransferHistogram>,
    uploads: Slot<TransferLeaderboard>,
    downloads: Slot<TransferLeaderboard>,
    directories: Slot<Vec<TransferDirectorySummary>>,
    upload_pareto: Slot<Vec<TransferErrorSummary>>,
    download_pareto: Slot<Vec<TransferErrorSummary>>,
    upload_errors: Slot<Vec<TransferErrorDetail>>,
    download_errors: Slot<Vec<TransferErrorDetail>>,
}

impl Reports {
    fn new() -> Self {
        Self {
            summary: RwSignal::new(None),
            histogram: RwSignal::new(None),
            uploads: RwSignal::new(None),
            downloads: RwSignal::new(None),
            directories: RwSignal::new(None),
            upload_pareto: RwSignal::new(None),
            download_pareto: RwSignal::new(None),
            upload_errors: RwSignal::new(None),
            download_errors: RwSignal::new(None),
        }
    }

    fn clear_tabs(self) {
        self.uploads.set(None);
        self.downloads.set(None);
        self.directories.set(None);
        self.upload_pareto.set(None);
        self.download_pareto.set(None);
        self.upload_errors.set(None);
        self.download_errors.set(None);
    }
}

struct ReportWindow {
    period: usize,
    start: String,
    end: String,
    requested_summary: bool,
    requested_tabs: HashSet<String>,
}

/// Show the last result while refreshing an older snapshot.
fn fill<T: serde::de::DeserializeOwned + Send + Sync + 'static>(
    slot: Slot<T>,
    generation: StoredValue<u64>,
    current: u64,
    cache: ReportCache,
    period: usize,
    request: reports::ReportRequest<T>,
) {
    let cached = request.cached(cache, period);
    let has_snapshot = cached.is_some();
    let fresh = cached.as_ref().is_some_and(|(_, fresh)| *fresh);
    slot.set(cached.map(|(report, _)| Ok(report)));
    if fresh {
        return;
    }
    spawn_local(async move {
        let result = request.fetch(cache, period).await;
        if generation.try_get_value() == Some(current) && (result.is_ok() || !has_snapshot) {
            let _ = slot.try_set(Some(result.map_err(|error| error.to_string())));
        }
    });
}

fn bytes(report: &TransferSummaryReport, direction: TransferDirection) -> u64 {
    report
        .get(&direction)
        .map(|states| states.values().map(|summary| summary.total_bytes).sum())
        .unwrap_or_default()
}
fn count(report: &TransferSummaryReport, direction: TransferDirection) -> u64 {
    report
        .get(&direction)
        .map(|states| states.values().map(|summary| summary.count).sum())
        .unwrap_or_default()
}
fn directory_label(path: Option<&str>) -> String {
    let path = path.unwrap_or_default();
    let parts = path
        .split(['/', '\\'])
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    parts[parts.len().saturating_sub(2)..].join("/")
}
fn pretty_bytes(bytes: u64) -> String {
    util::format_bytes(bytes as f64)
}
fn plural(count: u64, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

/// "just now", "5m ago", "3h ago", "2d ago", or the date when older than a month.
pub(crate) fn relative_time(time: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - time).num_seconds();
    let elapsed = seconds.unsigned_abs();
    let text = if elapsed < 60 {
        return "just now".to_owned();
    } else if elapsed < 3_600 {
        format!("{}m", elapsed / 60)
    } else if elapsed < 86_400 {
        format!("{}h", elapsed / 3_600)
    } else if elapsed < 30 * 86_400 {
        format!("{}d", elapsed / 86_400)
    } else {
        return time.with_timezone(&Local).format("%b %-d, %Y").to_string();
    };
    if seconds < 0 {
        format!("in {text}")
    } else {
        format!("{text} ago")
    }
}

/// A `<time>` with the relative time as text and the full local timestamp as its title.
pub(crate) fn time_ago(time: DateTime<Utc>) -> impl IntoView {
    let stamp = time.to_rfc3339();
    let full = util::format_date(&stamp);
    view! { <time class="nowrap" datetime=stamp title=full>{relative_time(time)}</time> }
}

#[component]
pub fn Dashboard() -> impl IntoView {
    let cache = expect_context::<ReportCache>();
    let period = RwSignal::new(2_usize);
    let data = Reports::new();
    let tab = RwSignal::new("users".to_owned());
    // Bumped per load so a slow response for an earlier period cannot overwrite a newer one.
    let generation = StoredValue::new(0_u64);

    Effect::new(move |previous: Option<ReportWindow>| {
        let period = period.get();
        let selected = PERIODS[period];
        let active_tab = tab.get();
        let previous = previous.filter(|window| window.period == period);
        let new_period = previous.is_none();
        let mut window = previous.unwrap_or_else(|| {
            // Share a window across page visits and sessions for the current minute.
            let now = Utc::now();
            let end = DateTime::from_timestamp((now.timestamp().div_euclid(60) + 1) * 60, 0)
                .unwrap_or(now);
            let start = selected
                .days
                .map(|days| end - Duration::days(days))
                .unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap_or(end));
            ReportWindow {
                period,
                start: start.to_rfc3339(),
                end: end.to_rfc3339(),
                requested_summary: false,
                requested_tabs: HashSet::new(),
            }
        });
        if new_period {
            generation.update_value(|current| *current = current.wrapping_add(1));
            data.clear_tabs();
        }
        let current = generation.get_value();
        macro_rules! load {
            ($slot:ident, $report:ident $(, $arg:expr)*) => {{
                let request = reports::$report(&window.start, &window.end $(, $arg)*);
                fill(data.$slot, generation, current, cache, period, request);
            }};
        }
        if new_period {
            load!(histogram, histogram, selected.buckets);
        }
        // Give the chart the first turn on SQLite's transfer connection.
        if data.histogram.with(Option::is_none) {
            return window;
        }
        if !window.requested_summary {
            window.requested_summary = true;
            load!(summary, summary);
        }
        if window.requested_tabs.insert(active_tab.clone()) {
            match active_tab.as_str() {
                "users" => {
                    load!(uploads, leaderboard, "upload", "count");
                    load!(downloads, leaderboard, "download", "count");
                }
                "content" => load!(directories, directories),
                _ => {
                    load!(upload_pareto, error_types, "upload");
                    load!(download_pareto, error_types, "download");
                    load!(upload_errors, errors, "upload");
                    load!(download_errors, errors, "download");
                }
            }
        }
        window
    });

    let period_items = PERIODS
        .iter()
        .map(|period| TabItem::new(period.label, period.label))
        .collect::<Vec<_>>();
    let period_value = Signal::derive(move || PERIODS[period.get()].label.to_owned());
    let pick_period = Callback::new(move |label: String| {
        if let Some(index) = PERIODS.iter().position(|period| period.label == label) {
            period.set(index);
        }
    });

    view! {
        <div class="dashboard stack">
            <PageHeader title="Overview" description="Live transfers, history and reports."/>
            <LiveStats/>
            <Card title="History" description="Completed transfers in the selected period." title_id="history-heading"
                class="history" actions=ViewFn::from(move || view! {
                    <Tabs items=period_items.clone() value=period_value on_change=pick_period label="History range"/>
                })>
                <div class="stack stack--lg">
                    <SummaryStats summary=data.summary/>
                    {move || slot_view(data.histogram.get(), chart_skeleton, |histogram| view! {
                        <HistoryChart histogram errors=false/>
                    }.into_any())}
                    <div class="stack">
                        <Tabs underline=true label="History reports" value=tab on_change=Callback::new(move |value| tab.set(value))
                            items=vec![
                                TabItem::new("users", "Users").icon("users"),
                                TabItem::new("content", "Content").icon("folder"),
                                TabItem::new("errors", "Errors").icon("circle-alert"),
                            ]/>
                        {move || match tab.get().as_str() {
                            "users" => leaderboard_view(data.uploads, data.downloads).into_any(),
                            "content" => directories_view(data.directories).into_any(),
                            _ => errors_view(data).into_any(),
                        }}
                    </div>
                </div>
            </Card>
        </div>
    }
}

/// The slot's content: a skeleton while loading, an alert on failure, `ready` once loaded.
fn slot_view<T>(
    value: Option<Result<T, String>>,
    loading: fn() -> AnyView,
    ready: impl Fn(T) -> AnyView,
) -> AnyView {
    match value {
        None => loading(),
        Some(Err(error)) => view! {
            <Alert variant="warning" title="This report could not be loaded">{error}</Alert>
        }
        .into_any(),
        Some(Ok(value)) => ready(value),
    }
}

fn chart_skeleton() -> AnyView {
    view! {
        <div class="chart chart--loading" role="status" aria-label="Loading chart">
            <span class="skeleton chart__skeleton"></span>
        </div>
    }
    .into_any()
}

fn table_skeleton(columns: usize) -> AnyView {
    view! {
        <div class="table-wrap" role="status" aria-label="Loading report">
            <table class="table table--compact">
                <tbody>
                    {(0..4).map(|_| view! {
                        <tr>{(0..columns).map(|_| view! { <td><span class="skeleton report-skeleton"></span></td> }).collect_view()}</tr>
                    }).collect_view()}
                </tbody>
            </table>
        </div>
    }
    .into_any()
}

#[component]
fn SummaryStats(summary: Slot<TransferSummaryReport>) -> impl IntoView {
    let tile = move |label: &'static str,
                     icon: &'static str,
                     class: &'static str,
                     values: fn(&TransferSummaryReport) -> (String, String)| {
        view! {
            <div class="stat summary-stat">
                <div class="summary-stat__head">
                    <div class="stat__label">{label}</div>
                    <Icon name=icon size="sm" class=class/>
                </div>
                {move || match summary.get() {
                    Some(Ok(report)) => {
                        let (value, hint) = values(&report);
                        view! {
                            <div class="stat__value stat__value--lg">{value}</div>
                            <div class="stat__hint">{hint}</div>
                        }.into_any()
                    }
                    Some(Err(_)) => view! { <div class="stat__value stat__value--lg muted">"—"</div><div class="stat__hint">"Unavailable"</div> }.into_any(),
                    None => view! {
                        <span class="skeleton summary-stat__skeleton" role="status" aria-label="Loading"></span>
                        <span class="skeleton summary-stat__skeleton summary-stat__skeleton--hint"></span>
                    }.into_any(),
                }}
            </div>
        }
    };
    view! {
        <div class="stats summary-stats" aria-label="Period summary">
            {tile("Downloaded", "download", "series-download", |report| (
                pretty_bytes(bytes(report, TransferDirection::Download)),
                plural(count(report, TransferDirection::Download), "file", "files"),
            ))}
            {tile("Uploaded", "upload", "series-upload", |report| (
                pretty_bytes(bytes(report, TransferDirection::Upload)),
                plural(count(report, TransferDirection::Upload), "file", "files"),
            ))}
            {tile("Share ratio", "arrow-up-down", "muted", |report| {
                let down = bytes(report, TransferDirection::Download);
                let up = bytes(report, TransferDirection::Upload);
                let ratio = match (up, down) { (_, 0) => "—".to_owned(), (up, down) if up > 0 && (up as f64) < down as f64 * 0.01 => "< 0.01".to_owned(), (up, down) => format!("{:.2}", up as f64 / down as f64) };
                (ratio, "Uploaded ÷ downloaded".into())
            })}
            {tile("Distinct peers", "users", "muted", |report| {
                let peers = |direction| report.get(&direction)
                    .and_then(|states| states.get(&TransferState::Succeeded))
                    .map_or(0, |summary| summary.distinct_users);
                let down = peers(TransferDirection::Download);
                let up = peers(TransferDirection::Upload);
                (format!("{down} down · {up} up"), "Successful transfers by direction".into())
            })}
        </div>
    }
}

fn format_speed(speed: f64) -> String {
    if speed < 1.0 || !speed.is_finite() {
        "0 B/s".to_owned()
    } else {
        format!("{}/s", util::format_bytes_with(speed, 1, 4, ' ').trim())
    }
}

/// Samples of the live speed kept for the sparklines (one per metrics push, about 1s).
const SPARK_SAMPLES: usize = 60;

/// Live transfer tiles: speed, active and queued files per direction, from the live
/// metrics the daemon pushes over the events socket.
#[component]
fn LiveStats() -> impl IntoView {
    let metrics = expect_context::<AppContext>().metrics;
    let history = RwSignal::new(VecDeque::<(f64, f64)>::with_capacity(SPARK_SAMPLES));
    Effect::new(move |_| {
        let sample = metrics.with(|m| {
            (
                m.downloads.in_progress.total_speed,
                m.uploads.in_progress.total_speed,
            )
        });
        history.update(|history| {
            if history.len() == SPARK_SAMPLES {
                history.pop_front();
            }
            history.push_back(sample);
        });
    });
    let spark = move |upload: bool| {
        Signal::derive(move || {
            history.with(|history| {
                history
                    .iter()
                    .map(|(down, up)| if upload { *up } else { *down })
                    .collect::<Vec<_>>()
            })
        })
    };
    let tile = move |label: &'static str,
                     icon: &'static str,
                     upload: bool,
                     value: fn(&CurrentMetrics) -> String,
                     extra: AnyView| {
        let class = if upload {
            "live-stat__icon series-upload"
        } else {
            "live-stat__icon series-download"
        };
        view! {
            <div class="stat live-stat">
                <div class="live-stat__head">
                    <div class="stat__label">{label}</div>
                    <Icon name=icon size="sm" class=class/>
                </div>
                <div class="stat__value stat__value--lg">{move || metrics.with(value)}</div>
                {extra}
            </div>
        }
    };
    let hint = move |text: fn(&CurrentMetrics) -> String| {
        view! { <div class="stat__hint">{move || metrics.with(text)}</div> }.into_any()
    };
    view! {
        <section class="stats live-stats" aria-label="Live transfers">
            {tile("Download speed", "download", false, |m| format_speed(m.downloads.in_progress.total_speed),
                view! { <Sparkline values=spark(false) class="series-download"/> }.into_any())}
            {tile("Active downloads", "activity", false, |m| m.downloads.in_progress.files.to_string(),
                hint(|m| plural(m.downloads.in_progress.users, "user", "users")))}
            {tile("Queued downloads", "clock", false, |m| m.downloads.queued.files.to_string(),
                hint(|m| format!("{} · {}", pretty_bytes(m.downloads.queued.bytes), plural(m.downloads.queued.users, "user", "users"))))}
            {tile("Upload speed", "upload", true, |m| format_speed(m.uploads.in_progress.total_speed),
                view! { <Sparkline values=spark(true) class="series-upload"/> }.into_any())}
            {tile("Active uploads", "activity", true, |m| m.uploads.in_progress.files.to_string(),
                hint(|m| plural(m.uploads.in_progress.users, "user", "users")))}
            {tile("Queued uploads", "clock", true, |m| m.uploads.queued.files.to_string(),
                hint(|m| format!("{} · {}", pretty_bytes(m.uploads.queued.bytes), plural(m.uploads.queued.users, "user", "users"))))}
        </section>
    }
}

fn empty_row(columns: usize, text: &'static str) -> AnyView {
    view! { <tr class="empty-row"><td colspan=columns.to_string()>{text}</td></tr> }.into_any()
}

fn leaderboard_view(
    uploads: Slot<TransferLeaderboard>,
    downloads: Slot<TransferLeaderboard>,
) -> impl IntoView {
    let base = config::url_base();
    let base = base.trim_end_matches('/').to_owned();
    let section = move |title: &'static str,
                        description: &'static str,
                        slot: Slot<TransferLeaderboard>| {
        let base = base.clone();
        view! {
            <section class="report">
                <div class="report__head">
                    <h3 class="report__title">{title}</h3>
                    <p class="report__description">{description}</p>
                </div>
                {move || {
                    let base = base.clone();
                    slot_view(slot.get(), || table_skeleton(4), move |rows| view! {
                        <div class="table-wrap">
                            <table class="table table--compact">
                                <thead><tr><th>"User"</th><th class="num">"Files"</th><th class="num">"Size"</th><th class="num">"Avg speed"</th></tr></thead>
                                <tbody>
                                    {if rows.is_empty() { empty_row(4, "No transfers in this period") } else {
                                        rows.into_iter().map(|row| {
                                            let username = row.username.unwrap_or_default();
                                            let href = format!("{base}/users?username={}", util::encode_uri_component(&username));
                                            view! {
                                                <tr>
                                                    <td class="report__user"><A href=href attr:class="truncate">{username}</A></td>
                                                    <td class="num">{row.count}</td>
                                                    <td class="num">{pretty_bytes(row.total_bytes)}</td>
                                                    <td class="num">{util::format_speed(row.average_speed)}</td>
                                                </tr>
                                            }
                                        }).collect_view().into_any()
                                    }}
                                </tbody>
                            </table>
                        </div>
                    }.into_any())
                }}
            </section>
        }
    };
    view! {
        <div class="grid grid--2 reports-grid">
            {section("Uploads", "Users you uploaded to most.", uploads)}
            {section("Downloads", "Users you downloaded from most.", downloads)}
        </div>
    }
}

fn directories_view(directories: Slot<Vec<TransferDirectorySummary>>) -> impl IntoView {
    view! {
        <section class="report">
            <div class="report__head">
                <h3 class="report__title">"Popular directories"</h3>
                <p class="report__description">"Your shared directories downloaded most often."</p>
            </div>
            {move || slot_view(directories.get(), || table_skeleton(4), |rows| view! {
                <div class="table-wrap">
                    <table class="table table--compact">
                        <thead><tr><th class="shrink num">"#"</th><th>"Directory"</th><th class="num">"Downloads"</th><th class="num">"Users"</th></tr></thead>
                        <tbody>
                            {if rows.is_empty() { empty_row(4, "No data for this period") } else {
                                rows.into_iter().enumerate().map(|(index, row)| {
                                    let path = row.directory.unwrap_or_default();
                                    let label = directory_label(Some(&path));
                                    view! {
                                        <tr>
                                            <td class="num muted">{index + 1}</td>
                                            <td class="report__path"><span class="mono truncate" title=path>{label}</span></td>
                                            <td class="num">{row.count}</td>
                                            <td class="num">{row.distinct_users}</td>
                                        </tr>
                                    }
                                }).collect_view().into_any()
                            }}
                        </tbody>
                    </table>
                </div>
            }.into_any())}
        </section>
    }
}

fn direction_badge(direction: TransferDirection) -> impl IntoView {
    match direction {
        TransferDirection::Download => {
            view! { <Badge variant="primary" icon="arrow-down" text="Download"/> }
        }
        TransferDirection::Upload => {
            view! { <Badge variant="neutral" icon="arrow-up" text="Upload"/> }
        }
    }
}

fn direction_tabs(value: RwSignal<String>, label: &'static str) -> impl IntoView {
    view! {
        <Tabs label=label value=value on_change=Callback::new(move |picked| value.set(picked))
            items=vec![TabItem::new("All", "All"), TabItem::new("Upload", "Uploads"), TabItem::new("Download", "Downloads")]/>
    }
}

/// Both directions of a report, merged by `order` and cut to ten when showing all.
fn pick<T: Clone + Send + Sync + 'static>(
    direction: &str,
    uploads: Slot<Vec<T>>,
    downloads: Slot<Vec<T>>,
    order: fn(&mut Vec<T>),
) -> Option<Result<Vec<T>, String>> {
    match direction {
        "Upload" => uploads.get(),
        "Download" => downloads.get(),
        _ => match (uploads.get(), downloads.get()) {
            (Some(Ok(mut both)), Some(Ok(more))) => {
                both.extend(more);
                order(&mut both);
                both.truncate(10);
                Some(Ok(both))
            }
            (Some(Err(error)), _) | (_, Some(Err(error))) => Some(Err(error)),
            _ => None,
        },
    }
}

fn errors_view(data: Reports) -> impl IntoView {
    let pareto_direction = RwSignal::new("All".to_owned());
    let recent_direction = RwSignal::new("All".to_owned());
    view! {
        <div class="stack stack--lg">
            {move || slot_view(data.histogram.get(), chart_skeleton, |histogram| view! {
                <HistoryChart histogram errors=true/>
            }.into_any())}
            <section class="report">
                <div class="report__head report__head--row">
                    <div>
                        <h3 class="report__title">"Errors by type"</h3>
                        <p class="report__description">"Failures grouped by their error message."</p>
                    </div>
                    {direction_tabs(pareto_direction, "Error type direction")}
                </div>
                {move || {
                    let rows = pick(&pareto_direction.get(), data.upload_pareto, data.download_pareto,
                        |rows| rows.sort_by_key(|row| std::cmp::Reverse(row.count)));
                    slot_view(rows, || table_skeleton(3), |rows| view! {
                        <div class="table-wrap">
                            <table class="table table--compact">
                                <thead><tr><th>"Error"</th><th class="num">"Count"</th><th class="num">"Users"</th></tr></thead>
                                <tbody>
                                    {if rows.is_empty() { empty_row(3, "No errors in this period") } else {
                                        rows.into_iter().map(|row| {
                                            let error = row.error.unwrap_or_default();
                                            view! {
                                                <tr>
                                                    <td class="report__error"><span class="truncate" title=error.clone()>{util::truncate(&error, 160)}</span></td>
                                                    <td class="num">{row.count}</td>
                                                    <td class="num">{row.distinct_users}</td>
                                                </tr>
                                            }
                                        }).collect_view().into_any()
                                    }}
                                </tbody>
                            </table>
                        </div>
                    }.into_any())
                }}
            </section>
            <section class="report">
                <div class="report__head report__head--row">
                    <div>
                        <h3 class="report__title">"Recent errors"</h3>
                        <p class="report__description">"The latest failed transfers."</p>
                    </div>
                    {direction_tabs(recent_direction, "Recent error direction")}
                </div>
                {move || {
                    let rows = pick(&recent_direction.get(), data.upload_errors, data.download_errors,
                        |rows| rows.sort_by_key(|row| std::cmp::Reverse(row.ended_at)));
                    slot_view(rows, || table_skeleton(5), |rows| view! {
                        <div class="table-wrap">
                            <table class="table table--compact">
                                <thead><tr><th>"When"</th><th>"Direction"</th><th>"User"</th><th>"File"</th><th>"Error"</th></tr></thead>
                                <tbody>
                                    {if rows.is_empty() { empty_row(5, "No errors in this period") } else {
                                        rows.into_iter().map(|row| {
                                            let exception = row.error.unwrap_or_default();
                                            let basename = util::get_file_name(&row.filename).to_owned();
                                            view! {
                                                <tr>
                                                    <td class="muted">{row.ended_at.map(|time| time_ago(time).into_any()).unwrap_or_else(|| "—".into_any())}</td>
                                                    <td>{direction_badge(row.direction)}</td>
                                                    <td class="nowrap">{row.username}</td>
                                                    <td class="report__file"><span class="truncate" title=row.filename>{basename}</span></td>
                                                    <td class="report__error"><span class="truncate" title=exception.clone()>{util::truncate(&exception, 160)}</span></td>
                                                </tr>
                                            }
                                        }).collect_view().into_any()
                                    }}
                                </tbody>
                            </table>
                        </div>
                    }.into_any())
                }}
            </section>
        </div>
    }
}
