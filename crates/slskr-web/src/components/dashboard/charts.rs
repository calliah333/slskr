//! The transfer history chart: stacked bars per histogram bucket (downloads below uploads),
//! HTML axis labels in theme text colours, a toggleable legend and a hover tooltip.
//! Series colours come from `--chart-download` / `--chart-upload` (pages/search.css).

use chrono::{DateTime, Local, Utc};
use leptos::prelude::*;
use slskr_types::{
    telemetry::{TransferHistogram, TransferSummaryReport},
    transfers::{TransferDirection, TransferState},
};

use crate::ui::EmptyState;

#[derive(Clone, Copy)]
struct Series {
    key: TransferDirection,
    label: &'static str,
    class: &'static str,
}

/// Bottom to top. Colour follows the direction in both charts.
fn series(errors: bool) -> [Series; 2] {
    [
        Series {
            key: TransferDirection::Download,
            label: if errors {
                "Download errors"
            } else {
                "Downloaded"
            },
            class: "chart__swatch--download",
        },
        Series {
            key: TransferDirection::Upload,
            label: if errors { "Upload errors" } else { "Uploaded" },
            class: "chart__swatch--upload",
        },
    ]
}

fn value(report: &TransferSummaryReport, direction: TransferDirection, errors: bool) -> f64 {
    let Some(states) = report.get(&direction) else {
        return 0.0;
    };
    let amount: u64 = if errors {
        [
            TransferState::Errored,
            TransferState::Cancelled,
            TransferState::TimedOut,
        ]
        .iter()
        .filter_map(|state| states.get(state))
        .map(|summary| summary.count)
        .sum()
    } else {
        states.values().map(|summary| summary.total_bytes).sum()
    };
    amount as f64
}

const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];

/// A 1/2/5 × 10ⁿ step that splits `range` into about `ticks` parts.
fn nice_step(range: f64, ticks: f64) -> f64 {
    let raw = (range / ticks).max(f64::MIN_POSITIVE);
    let magnitude = 10f64.powf(raw.log10().floor());
    let normal = raw / magnitude;
    let nice = if normal <= 1.0 {
        1.0
    } else if normal <= 2.0 {
        2.0
    } else if normal <= 5.0 {
        5.0
    } else {
        10.0
    };
    nice * magnitude
}

fn trim_number(value: f64) -> String {
    let text = format!("{value:.2}");
    text.trim_end_matches('0').trim_end_matches('.').to_owned()
}

/// The y scale: its top value and the tick labels from 0 up, with round numbers in one unit.
struct Scale {
    top: f64,
    ticks: Vec<(f64, String)>,
}

fn scale(maximum: f64, bytes: bool) -> Scale {
    let maximum = maximum.max(1.0);
    let (unit, name) = if bytes {
        let power = ((maximum.ln() / 1024f64.ln()).floor() as usize).min(UNITS.len() - 1);
        (1024f64.powi(power as i32), UNITS[power])
    } else {
        (1.0, "")
    };
    let scaled = maximum / unit;
    let mut step = nice_step(scaled, 4.0);
    if !bytes {
        step = step.max(1.0);
    }
    let steps = (scaled / step).ceil().max(1.0) as usize;
    let ticks = (0..=steps)
        .map(|index| {
            let tick = index as f64 * step;
            let label = if bytes {
                format!("{} {name}", trim_number(tick))
            } else {
                trim_number(tick)
            };
            (tick * unit, label)
        })
        .collect();
    Scale {
        top: steps as f64 * step * unit,
        ticks,
    }
}

fn format_amount(amount: f64, bytes: bool) -> String {
    if bytes {
        crate::services::util::format_bytes(amount)
    } else {
        format!("{}", amount as u64)
    }
}

/// Axis and tooltip date formats for the span shown.
fn date_formats(first: DateTime<Utc>, last: DateTime<Utc>) -> (&'static str, &'static str) {
    let span = last - first;
    if span <= chrono::Duration::days(2) {
        ("%H:%M", "%b %-d, %H:%M")
    } else if span <= chrono::Duration::days(14) {
        ("%a %-d", "%a %b %-d, %H:%M")
    } else if span <= chrono::Duration::days(100) {
        ("%b %-d", "%b %-d, %H:%M")
    } else if span <= chrono::Duration::days(400) {
        ("%b %-d", "%b %-d, %Y")
    } else {
        ("%b %Y", "%b %-d, %Y")
    }
}

#[component]
pub fn HistoryChart(histogram: TransferHistogram, errors: bool) -> impl IntoView {
    let definitions = series(errors);
    let stamps = histogram.keys().copied().collect::<Vec<_>>();
    let amounts = histogram
        .values()
        .map(|report| definitions.map(|series| value(report, series.key, errors)))
        .collect::<Vec<_>>();
    let empty = amounts.iter().flatten().all(|amount| *amount <= 0.0);
    let title = if errors {
        "Failed transfers over time"
    } else {
        "Transfer volume over time"
    };
    if stamps.is_empty() || empty {
        let message = if errors {
            "No failed transfers in this period"
        } else {
            "No transfers in this period"
        };
        return view! {
            <div class="chart chart--empty" aria-label=title>
                <EmptyState icon="chart-column" title=message/>
            </div>
        }
        .into_any();
    }
    let bytes = !errors;
    let count = amounts.len();
    let amounts = StoredValue::new(amounts);
    let stamps = StoredValue::new(stamps);
    let visible = RwSignal::new([true, true]);
    let hovered = RwSignal::new(None::<usize>);
    let (axis_format, tip_format) = stamps.with_value(|stamps| {
        date_formats(
            stamps.first().copied().unwrap_or_default(),
            stamps.last().copied().unwrap_or_default(),
        )
    });
    let scale = Memo::new(move |_| {
        let shown = visible.get();
        let maximum = amounts.with_value(|amounts| {
            amounts
                .iter()
                .map(|row| {
                    row.iter()
                        .zip(shown)
                        .filter(|(_, on)| *on)
                        .map(|(value, _)| *value)
                        .sum::<f64>()
                })
                .fold(0.0, f64::max)
        });
        let scale = scale(maximum, bytes);
        (scale.top, scale.ticks)
    });
    let totals = amounts.with_value(|amounts| {
        [0, 1].map(|index| amounts.iter().map(|row| row[index]).sum::<f64>())
    });
    let summary = format!(
        "{title}: {} {}, {} {}",
        definitions[0].label.to_lowercase(),
        format_amount(totals[0], bytes),
        definitions[1].label.to_lowercase(),
        format_amount(totals[1], bytes),
    );
    let percent = move |amount: f64| {
        let top = scale.with(|(top, _)| *top);
        if top > 0.0 {
            (amount / top * 100.0).clamp(0.0, 100.0)
        } else {
            0.0
        }
    };
    // About five date labels; the 2nd and 4th are hidden on phones.
    let labels = {
        let wanted = count.clamp(1, 5);
        let mut indices = (0..wanted)
            .map(|k| {
                if wanted == 1 {
                    0
                } else {
                    (k * (count - 1) + (wanted - 1) / 2) / (wanted - 1)
                }
            })
            .collect::<Vec<_>>();
        indices.dedup();
        let last = indices.len().saturating_sub(1);
        indices
            .into_iter()
            .enumerate()
            .map(|(position, index)| {
                let text = stamps.with_value(|stamps| {
                    stamps[index]
                        .with_timezone(&Local)
                        .format(axis_format)
                        .to_string()
                });
                let class = match position {
                    0 => "chart__x-label chart__x-label--start",
                    p if p == last => "chart__x-label chart__x-label--end",
                    p if p % 2 == 1 => "chart__x-label hide-sm",
                    _ => "chart__x-label",
                };
                let left = format!("{}%", (index as f64 + 0.5) / count as f64 * 100.0);
                view! { <span class=class style:left=left>{text}</span> }
            })
            .collect_view()
    };

    view! {
        <figure class="chart" aria-label=title>
            <div class="chart__legend" role="group" aria-label="Series">
                {definitions.iter().enumerate().map(|(index, series)| {
                    let series = *series;
                    view! {
                        <button type="button" class="chart__legend-item"
                            aria-pressed=move || visible.with(|shown| shown[index]).to_string()
                            title=move || if visible.with(|shown| shown[index]) { format!("Hide {}", series.label) } else { format!("Show {}", series.label) }
                            on:click=move |_| visible.update(|shown| {
                                // Keep at least one series on.
                                if !shown[index] || shown[1 - index] { shown[index] = !shown[index]; }
                            })>
                            <span class=format!("chart__swatch {}", series.class) aria-hidden="true"></span>
                            {series.label}
                            <span class="chart__legend-total tabular">{format_amount(totals[index], bytes)}</span>
                        </button>
                    }
                }).collect_view()}
            </div>
            <div class="chart__body">
                <div class="chart__y" aria-hidden="true">
                    {move || scale.with(|(top, ticks)| {
                        let top = *top;
                        ticks.iter().map(|(tick, label)| {
                            let bottom = if top > 0.0 { tick / top * 100.0 } else { 0.0 };
                            view! { <span class="chart__y-label tabular" style:bottom=format!("{bottom}%")>{label.clone()}</span> }
                        }).collect_view()
                    })}
                </div>
                <div class="chart__plot" role="img" aria-label=summary on:mouseleave=move |_| hovered.set(None)>
                    {move || scale.with(|(top, ticks)| {
                        let top = *top;
                        ticks.iter().map(|(tick, _)| {
                            let bottom = if top > 0.0 { tick / top * 100.0 } else { 0.0 };
                            view! { <span class="chart__grid" class:chart__grid--base=*tick == 0.0 style:bottom=format!("{bottom}%")></span> }
                        }).collect_view()
                    })}
                    <div class="chart__bars">
                        {(0..count).map(|index| {
                            let row = amounts.with_value(|amounts| amounts[index]);
                            view! {
                                <div class="chart__col" class:is-hovered=move || hovered.get() == Some(index)
                                    on:mouseenter=move |_| hovered.set(Some(index))
                                    on:click=move |_| hovered.set(Some(index))>
                                    {definitions.iter().enumerate().map(|(slot, series)| {
                                        let class = format!("chart__bar {}", series.class);
                                        let amount = row[slot];
                                        view! {
                                            <span class=class
                                                style:height=move || {
                                                    let shown = visible.with(|shown| shown[slot]);
                                                    format!("{}%", if shown { percent(amount) } else { 0.0 })
                                                }
                                                class:is-empty=move || amount <= 0.0 || !visible.with(|shown| shown[slot])></span>
                                        }
                                    }).collect_view()}
                                </div>
                            }
                        }).collect_view()}
                    </div>
                    {move || hovered.get().map(|index| {
                        let row = amounts.with_value(|amounts| amounts[index]);
                        let when = stamps.with_value(|stamps| stamps[index].with_timezone(&Local).format(tip_format).to_string());
                        let x = (index as f64 + 0.5) / count as f64 * 100.0;
                        let (side, offset) = if x > 50.0 { ("right", 100.0 - x) } else { ("left", x) };
                        let style = format!("{side}: calc({offset}% - 0.5rem)");
                        view! {
                            <div class="chart__tooltip" style=style role="presentation">
                                <div class="chart__tooltip-title">{when}</div>
                                {definitions.iter().enumerate().filter(|(slot, _)| visible.with(|shown| shown[*slot])).map(|(slot, series)| view! {
                                    <div class="chart__tooltip-row">
                                        <span class=format!("chart__swatch {}", series.class) aria-hidden="true"></span>
                                        <span class="grow">{series.label}</span>
                                        <span class="font-medium tabular">{format_amount(row[slot], bytes)}</span>
                                    </div>
                                }).collect_view()}
                            </div>
                        }
                    })}
                </div>
                <div class="chart__x" aria-hidden="true">{labels}</div>
            </div>
            <figcaption class="sr-only">{title}</figcaption>
        </figure>
    }
    .into_any()
}

/// A tiny line of recent values (live speed tiles). `values` are oldest first.
#[component]
pub fn Sparkline(#[prop(into)] values: Signal<Vec<f64>>, class: &'static str) -> impl IntoView {
    const WIDTH: f64 = 100.0;
    const HEIGHT: f64 = 24.0;
    let points = move || {
        values.with(|values| {
            let maximum = values.iter().copied().fold(0.0, f64::max);
            let step = WIDTH / (values.len().max(2) - 1) as f64;
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    let y = if maximum > 0.0 {
                        HEIGHT - 1.0 - value / maximum * (HEIGHT - 2.0)
                    } else {
                        HEIGHT - 1.0
                    };
                    format!("{:.1},{:.1}", index as f64 * step, y)
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
    };
    view! {
        <svg class=format!("sparkline {class}") viewBox="0 0 100 24" preserveAspectRatio="none" aria-hidden="true">
            <polyline points=points fill="none" stroke="currentColor" stroke-width="1.5"
                vector-effect="non-scaling-stroke" stroke-linejoin="round" stroke-linecap="round"/>
        </svg>
    }
}
