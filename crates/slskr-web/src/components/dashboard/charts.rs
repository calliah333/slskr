use leptos::prelude::*;
use slskr_api::telemetry::TransferHistogram;

#[derive(Clone, Debug)]
pub struct Series {
    pub key: &'static str,
    pub label: &'static str,
    pub color: &'static str,
}

fn value(
    report: &slskr_api::telemetry::TransferSummaryReport,
    direction: &str,
    errors: bool,
) -> f64 {
    let Some(states) = report.get(direction) else {
        return 0.0;
    };
    let amount: i64 = if errors {
        ["Errored", "Cancelled", "TimedOut"]
            .iter()
            .filter_map(|state| states.get(*state))
            .map(|summary| summary.count)
            .sum()
    } else {
        states.values().map(|summary| summary.total_bytes).sum()
    };
    amount.max(0) as f64
}

#[component]
pub fn HistoryChart(histogram: TransferHistogram, errors: bool) -> impl IntoView {
    let definitions = if errors {
        [
            Series {
                key: "Upload",
                label: "Upload errors",
                color: "#db2828",
            },
            Series {
                key: "Download",
                label: "Download errors",
                color: "#f2711c",
            },
        ]
    } else {
        [
            Series {
                key: "Upload",
                label: "Uploaded",
                color: "#21ba45",
            },
            Series {
                key: "Download",
                label: "Downloaded",
                color: "#2185d0",
            },
        ]
    };
    let visible = RwSignal::new(
        definitions
            .iter()
            .map(|series| series.key)
            .collect::<Vec<_>>(),
    );
    let amounts = histogram
        .values()
        .map(|report| {
            definitions
                .each_ref()
                .map(|series| value(report, series.key, errors))
        })
        .collect::<Vec<_>>();
    let maximum = amounts.iter().flatten().copied().fold(1.0_f64, f64::max);
    let width = 670.0 / amounts.len().max(1) as f64;
    view! {
        <section class="dashboard-chart" aria-label=if errors { "Transfer errors over time" } else { "Transfer volume over time" }>
            <div class="chart-legend">
                {definitions.iter().map(|series| {
                    let key = series.key;
                    let label = series.label;
                    let color = series.color;
                    view! { <button type="button" class="chart-series" aria-pressed=move || visible.with(|active| active.contains(&key)).to_string() style:color=color on:click=move |_| {
                        visible.update(|active| if active.contains(&key) { active.retain(|item| *item != key); } else { active.push(key); });
                    }><span class="chart-key" style:background-color=color></span>{label}</button> }
                }).collect_view()}
            </div>
            <svg viewBox="0 0 720 220" role="img" aria-label="Interactive transfer history chart" class="history-chart">
                <line x1="40" y1="190" x2="710" y2="190" stroke="currentColor" opacity="0.3" />
                {amounts.into_iter().enumerate().flat_map(|(index, amounts)| {
                    let x = 40.0 + index as f64 * width;
                    definitions.iter().zip(amounts).map(move |(series, amount)| {
                        let key = series.key;
                        let height = (amount / maximum * 170.0).max(if amount > 0.0 { 1.0 } else { 0.0 });
                        let title = format!("{}: {}", series.label, amount as i64);
                        view! { <rect x=x y=(190.0-height) width=(width.max(1.0) * 0.85) height=height fill=series.color opacity="0.78"
                            style:display=move || if visible.with(|active| active.contains(&key)) { "" } else { "none" }>
                            <title>{title}</title>
                        </rect> }
                    })
                }).collect_view()}
                {histogram.keys().next().map(|stamp| view! { <text x="40" y="210" font-size="11">{stamp.format("%Y-%m-%d %H:%M").to_string()}</text> })}
                {histogram.keys().next_back().map(|stamp| view! { <text x="560" y="210" font-size="11">{stamp.format("%Y-%m-%d %H:%M").to_string()}</text> })}
            </svg>
            {histogram.is_empty().then(|| view! { <p class="muted">No history available for this period.</p> })}
        </section>
    }
}
