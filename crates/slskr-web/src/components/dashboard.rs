mod charts;
mod reports;

use chrono::{Duration, Utc};
use leptos::prelude::*;
use leptos_router::components::A;
use leptos_router::hooks::use_navigate;
use slskr_types::{
    telemetry::{
        TransferDirectorySummary, TransferErrorDetail, TransferErrorSummary, TransferHistogram,
        TransferLeaderboard, TransferSummaryReport,
    },
    transfers::{TransferDirection, TransferState},
};
use wasm_bindgen_futures::spawn_local;

use crate::{
    config,
    services::{api, util},
};
use charts::HistoryChart;

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

#[derive(Clone, Default)]
struct DashboardData {
    summary: TransferSummaryReport,
    histogram: TransferHistogram,
    uploads: TransferLeaderboard,
    downloads: TransferLeaderboard,
    directories: Vec<TransferDirectorySummary>,
    upload_pareto: Vec<TransferErrorSummary>,
    download_pareto: Vec<TransferErrorSummary>,
    upload_errors: Vec<TransferErrorDetail>,
    download_errors: Vec<TransferErrorDetail>,
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

#[component]
pub fn Dashboard() -> impl IntoView {
    let search = NodeRef::<leptos::html::Input>::new();
    let navigate = use_navigate();
    let text = RwSignal::new(String::new());
    let creating = RwSignal::new(false);
    let search_error = RwSignal::new(None::<String>);
    let period = RwSignal::new(2_usize);
    let loading = RwSignal::new(true);
    let load_error = RwSignal::new(None::<String>);
    let data = RwSignal::new(DashboardData::default());
    let tab = RwSignal::new(0_usize);
    let pareto_direction = RwSignal::new("All".to_owned());
    let recent_direction = RwSignal::new("All".to_owned());
    // Bumped per load so a slow response for an earlier period cannot overwrite a newer one.
    let generation = StoredValue::new(0_u64);

    Effect::new(move |_| {
        let selected = PERIODS[period.get()];
        let end = Utc::now();
        let start = selected
            .days
            .map(|days| end - Duration::days(days))
            .unwrap_or_else(|| DateTimeEpoch::value());
        let start_text = start.to_rfc3339();
        let end_text = end.to_rfc3339();
        let current = generation.get_value().wrapping_add(1);
        generation.set_value(current);
        loading.set(true);
        load_error.set(None);
        spawn_local(async move {
            let (
                summary,
                histogram,
                uploads,
                downloads,
                directories,
                upload_pareto,
                download_pareto,
                upload_errors,
                download_errors,
            ) = futures_all(
                reports::summary(&start_text, &end_text),
                reports::histogram(&start_text, &end_text, selected.buckets),
                reports::leaderboard(&start_text, &end_text, "upload", "count"),
                reports::leaderboard(&start_text, &end_text, "download", "count"),
                reports::directories(&start_text, &end_text),
                reports::error_types(&start_text, &end_text, "upload"),
                reports::error_types(&start_text, &end_text, "download"),
                reports::errors(&start_text, &end_text, "upload"),
                reports::errors(&start_text, &end_text, "download"),
            )
            .await;
            // Period changed or page unmounted while loading: drop the stale result.
            if generation.try_get_value() != Some(current) {
                return;
            }
            let failed = [
                summary.is_err(),
                histogram.is_err(),
                uploads.is_err(),
                downloads.is_err(),
                directories.is_err(),
                upload_pareto.is_err(),
                download_pareto.is_err(),
                upload_errors.is_err(),
                download_errors.is_err(),
            ]
            .into_iter()
            .any(|failed| failed);
            let _ = data.try_set(DashboardData {
                summary: summary.unwrap_or_default(),
                histogram: histogram.unwrap_or_default(),
                uploads: uploads.unwrap_or_default(),
                downloads: downloads.unwrap_or_default(),
                directories: directories.unwrap_or_default(),
                upload_pareto: upload_pareto.unwrap_or_default(),
                download_pareto: download_pareto.unwrap_or_default(),
                upload_errors: upload_errors.unwrap_or_default(),
                download_errors: download_errors.unwrap_or_default(),
            });
            if failed {
                let _ =
                    load_error.try_set(Some("Some dashboard reports could not be loaded.".into()));
            }
            let _ = loading.try_set(false);
        });
    });

    let create_search = move |open: bool| {
        let phrase = text.get_untracked().trim().to_owned();
        if phrase.is_empty() || creating.get_untracked() {
            return;
        }
        creating.set(true);
        search_error.set(None);
        let id = format!(
            "{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}",
            (js_sys::Math::random() * u32::MAX as f64) as u32,
            (js_sys::Math::random() * u16::MAX as f64) as u16,
            (js_sys::Math::random() * 4096.0) as u16,
            (js_sys::Math::random() * 4096.0) as u16,
            (js_sys::Math::random() * 0x1_0000_0000_0000u64 as f64) as u64,
        );
        let navigate = navigate.clone();
        spawn_local(async move {
            let request = slskr_types::search::SearchRequest {
                id: Some(id.clone()),
                query: phrase,
                ..Default::default()
            };
            match api::post::<_, serde_json::Value>("/searches", &request).await {
                Ok(_) => {
                    text.set(String::new());
                    creating.set(false);
                    if open {
                        navigate(
                            &format!("/searches/{}", util::encode_uri_component(&id)),
                            Default::default(),
                        );
                    }
                }
                Err(error) => {
                    search_error.set(Some(error.to_string()));
                    creating.set(false);
                }
            }
        });
    };
    let submit_search = create_search.clone();

    view! {
        <main class="dashboard" on:keydown=move |ev: web_sys::KeyboardEvent| {
            if ev.key() == "/" && !ev.ctrl_key() && !ev.meta_key() && !ev.alt_key() {
                if let Some(input) = search.get() { ev.prevent_default(); let _ = input.focus(); }
            }
        }>
            <section class="ui raised segment search-segment" aria-label="Search">
                <div class="search-segment-icon" aria-hidden="true">"⌕"</div>
                <form class="search-input" on:submit=move |ev| { ev.prevent_default(); submit_search(true); }>
                    <label class="sr-only" for="dashboard-search">"Search phrase"</label>
                    <input id="dashboard-search" node_ref=search type="search" placeholder="Search phrase (press / to focus)" prop:value=move || text.get()
                        on:input=move |ev| text.set(event_target_value(&ev)) />
                    <button type="button" disabled=move || creating.get() on:click=move |_| create_search(false)>"Start search"</button>
                    <button type="submit" disabled=move || creating.get()>"Search"</button>
                </form>
                {move || search_error.get().map(|error| view! { <p class="ui negative message" role="alert">{error}</p> })}
            </section>
            <section class="ui segment" aria-labelledby="history-heading">
                <div class="dashboard-heading"><h2 id="history-heading">"History"</h2>
                    <nav class="ui mini buttons" aria-label="History range">
                        {PERIODS.iter().enumerate().map(|(index, item)| view! { <button type="button" class="ui button" class:active=move || period.get() == index on:click=move |_| period.set(index)>{item.label}</button> }).collect_view()}
                    </nav>
                </div>
                {move || if loading.get() { view! { <p role="status">"Loading reports…"</p> }.into_any() } else {
                    let current = data.get();
                    let down = bytes(&current.summary, TransferDirection::Download);
                    let up = bytes(&current.summary, TransferDirection::Upload);
                    let peer_count = current.summary.get(&TransferDirection::Upload).and_then(|d| d.get(&TransferState::Succeeded)).map(|x| x.distinct_users).unwrap_or_default()
                        + current.summary.get(&TransferDirection::Download).and_then(|d| d.get(&TransferState::Succeeded)).map(|x| x.distinct_users).unwrap_or_default();
                    let ratio = if down > 0 { format!("{:.2}", up as f64 / down as f64) } else { "—".into() };
                    view! { <>
                        {move || load_error.get().map(|error| view! { <p class="ui warning message" role="status">{error}</p> })}
                        <div class="ui four small statistics">
                            <div class="blue statistic"><div class="value">{pretty_bytes(down)}</div><div class="label">"Downloaded · "{count(&current.summary, TransferDirection::Download).to_string()}{" files"}</div></div>
                            <div class="green statistic"><div class="value">{pretty_bytes(up)}</div><div class="label">"Uploaded · "{count(&current.summary, TransferDirection::Upload).to_string()}{" files"}</div></div>
                            <div class="statistic"><div class="value">{ratio}</div><div class="label">"Share ratio (↑/↓)"</div></div>
                            <div class="statistic"><div class="value">{peer_count}</div><div class="label">"Distinct peers"</div></div>
                        </div>
                        <HistoryChart histogram=current.histogram.clone() errors=false />
                        <nav class="ui pointing secondary menu" aria-label="History reports">
                            <button type="button" class="item" class:active=move || tab.get()==0 on:click=move |_| tab.set(0)>"Users"</button>
                            <button type="button" class="item" class:active=move || tab.get()==1 on:click=move |_| tab.set(1)>"Content"</button>
                            <button type="button" class="item" class:active=move || tab.get()==2 on:click=move |_| tab.set(2)>"Errors"</button>
                        </nav>
                        {move || match tab.get() {
                            0 => leaderboard_view(current.uploads.clone(), current.downloads.clone()).into_any(),
                            1 => directories_view(current.directories.clone()).into_any(),
                            _ => errors_view(current.clone(), pareto_direction, recent_direction).into_any(),
                        }}
                    </> }.into_any()
                }}
            </section>
        </main>
    }
}

async fn futures_all<A, B, C, D, E, F, G, H, I>(
    a: A,
    b: B,
    c: C,
    d: D,
    e: E,
    f: F,
    g: G,
    h: H,
    i: I,
) -> (
    A::Output,
    B::Output,
    C::Output,
    D::Output,
    E::Output,
    F::Output,
    G::Output,
    H::Output,
    I::Output,
)
where
    A: Future,
    B: Future,
    C: Future,
    D: Future,
    E: Future,
    F: Future,
    G: Future,
    H: Future,
    I: Future,
{
    // Concurrently, not one request after another.
    futures::join!(a, b, c, d, e, f, g, h, i)
}

struct DateTimeEpoch;
impl DateTimeEpoch {
    fn value() -> chrono::DateTime<Utc> {
        chrono::DateTime::from_timestamp(0, 0).unwrap_or_else(Utc::now)
    }
}

fn leaderboard_view(uploads: TransferLeaderboard, downloads: TransferLeaderboard) -> impl IntoView {
    let base = config::url_base();
    let base = base.trim_end_matches('/');
    view! { <div class="ui two column grid">
        {[("Uploads", uploads), ("Downloads", downloads)].into_iter().map(|(title, rows)| view! { <section class="column"><h3>{title}</h3><table class="ui compact unstackable table"><thead><tr><th>"Username"</th><th>"Count"</th><th>"Total size"</th><th>"Avg speed"</th></tr></thead><tbody>
            {rows.into_iter().map(|row| {
                let username = row.username.unwrap_or_default();
                let href = format!("{base}/users?username={}", util::encode_uri_component(&username));
                view! { <tr><td><A href=href>{username}</A></td><td>{row.count}</td><td>{pretty_bytes(row.total_bytes)}</td><td>{util::format_speed(row.average_speed)}</td></tr> }
            }).collect_view()}
        </tbody></table></section> }).collect_view()}
    </div> }
}

fn directories_view(rows: Vec<TransferDirectorySummary>) -> impl IntoView {
    view! { <section><h3>"Directories"</h3><table class="ui compact unstackable table"><thead><tr><th>#</th><th>"Directory"</th><th>"Downloads"</th><th>"Distinct users"</th></tr></thead><tbody>
        {if rows.is_empty() { view! { <tr><td colspan="4">"No data to display"</td></tr> }.into_any() } else { rows.into_iter().enumerate().map(|(index,row)| { let path = row.directory.unwrap_or_default(); let label=directory_label(Some(&path)); view! { <tr><td>{index+1}</td><td title=path>{label}</td><td>{row.count}</td><td>{row.distinct_users}</td></tr> } }).collect_view().into_any()}}
    </tbody></table></section> }
}

fn errors_view(
    data: DashboardData,
    pareto_direction: RwSignal<String>,
    recent_direction: RwSignal<String>,
) -> impl IntoView {
    let upload_histogram = data.histogram.clone();
    view! { <section><h3>"Transfer errors"</h3><HistoryChart histogram=upload_histogram errors=true />
        <h4>"Error count by type"</h4>
        <div class="ui mini buttons" aria-label="Error type direction">{["Upload","Download","All"].into_iter().map(|direction| view! { <button type="button" class="ui button" class:active=move || pareto_direction.get()==direction on:click=move |_| pareto_direction.set(direction.to_owned())>{direction}</button> }).collect_view()}</div>
        {move || {
            let rows = match pareto_direction.get().as_str() { "Upload" => data.upload_pareto.clone(), "Download" => data.download_pareto.clone(), _ => { let mut both=data.upload_pareto.clone(); both.extend(data.download_pareto.clone()); both.sort_by(|a,b| b.count.cmp(&a.count)); both.truncate(10); both } };
            view! { <table class="ui compact unstackable table"><thead><tr><th>"Exception"</th><th>"Count"</th><th>"Distinct users"</th></tr></thead><tbody>
                {if rows.is_empty() { view! { <tr><td colspan="3">"No data to display"</td></tr> }.into_any() } else { rows.into_iter().map(|row| { let exception=row.error.unwrap_or_default(); view! { <tr><td title=exception>{util::truncate(&exception,80)}</td><td>{row.count}</td><td>{row.distinct_users}</td></tr> } }).collect_view().into_any()}}
            </tbody></table> }
        }}
        <h4>"Recent errors"</h4>
        <div class="ui mini buttons" aria-label="Recent error direction">{["Upload","Download","All"].into_iter().map(|direction| view! { <button type="button" class="ui button" class:active=move || recent_direction.get()==direction on:click=move |_| recent_direction.set(direction.to_owned())>{direction}</button> }).collect_view()}</div>
        {move || {
            let rows = match recent_direction.get().as_str() { "Upload" => data.upload_errors.clone(), "Download" => data.download_errors.clone(), _ => { let mut both=data.upload_errors.clone(); both.extend(data.download_errors.clone()); both.sort_by(|a,b| b.ended_at.cmp(&a.ended_at)); both.truncate(10); both } };
            view! { <table class="ui compact unstackable table"><thead><tr><th>"Time"</th><th>"Direction"</th><th>"Username"</th><th>"Filename"</th><th>"Exception"</th></tr></thead><tbody>
                {if rows.is_empty() { view! { <tr><td colspan="5">"No data to display"</td></tr> }.into_any() } else { rows.into_iter().map(|row| {
                    let filename = row.filename;
                    let exception = row.error.unwrap_or_default();
                    let ended_at = row.ended_at.map_or_else(|| "—".to_owned(), |time| time.to_rfc3339());
                    let basename = util::get_file_name(&filename).to_owned();
                    view! { <tr><td>{ended_at}</td><td>{format!("{:?}", row.direction)}</td><td>{row.username}</td><td title=filename>{basename}</td><td title=exception>{util::truncate(&exception,80)}</td></tr> }
                }).collect_view().into_any()}}
            </tbody></table> }
        }}
    </section> }
}
