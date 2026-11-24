//! System → Logs: the live log (from the `logs` topic) and the daily log files.
//!
//! Records arrive one event each and can come in bursts. They are collected in a buffer
//! and moved into the view at most every [`FLUSH_MS`], which keeps at most [`MAX_LINES`]
//! rows; rows are keyed, so a flush only renders the new ones.

use std::time::Duration;

use leptos::{html::Div, prelude::*, task::spawn_local_scoped_with_cancellation};
use slskr_types::logs::{LogFile, LogRecord};

use super::format;
use crate::{
    config,
    services::{
        api,
        events::{Event, Topic, use_events},
        util::{download_file, encode_uri_component, format_bytes},
    },
    ui::{Alert, Card, EmptyState, Icon, Modal},
};

const FLUSH_MS: u64 = 250;
const MAX_LINES: usize = 1000;

/// Levels by severity; unknown levels count as info.
const LEVELS: [&str; 5] = ["trace", "debug", "info", "warn", "error"];

fn severity(level: &str) -> usize {
    LEVELS.iter().position(|l| *l == level).unwrap_or(2)
}

#[derive(Clone)]
struct Line {
    id: u64,
    level: &'static str,
    record: LogRecord,
}

/// Lines are immutable once created: the id identifies them.
impl PartialEq for Line {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

fn normalize_level(level: Option<&str>) -> &'static str {
    let level = level.unwrap_or("info").to_ascii_lowercase();
    match level.as_str() {
        "warning" => "warn",
        other => LEVELS
            .iter()
            .find(|l| **l == other)
            .copied()
            .unwrap_or("info"),
    }
}

fn level_badge(level: &str) -> &'static str {
    match level {
        "info" => "badge--primary",
        "warn" => "badge--warning",
        "error" => "badge--danger",
        _ => "",
    }
}

/// Lines waiting for the next flush.
#[derive(Default)]
struct Pending {
    lines: Vec<Line>,
    /// A snapshot arrived: the view is replaced rather than extended.
    replace: bool,
    scheduled: bool,
}

fn cap(lines: &mut Vec<Line>) {
    if lines.len() > MAX_LINES {
        lines.drain(..lines.len() - MAX_LINES);
    }
}

#[component]
pub fn Logs() -> impl IntoView {
    view! {
        <div class="system-logs stack">
            <LiveLog/>
            <LogFiles/>
        </div>
    }
}

#[component]
fn LiveLog() -> impl IntoView {
    let lines = RwSignal::new(Vec::<Line>::new());
    let pending = StoredValue::new_local(Pending::default());
    let next_id = StoredValue::new(0_u64);
    let waiting = RwSignal::new(0_usize);
    let paused = RwSignal::new(false);
    let received = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let min_level = RwSignal::new(String::from("trace"));
    let query = RwSignal::new(String::new());
    let view_ref = NodeRef::<Div>::new();
    let at_bottom = StoredValue::new(true);

    let scroll_to_end = move || {
        request_animation_frame(move || {
            if let Some(el) = view_ref.try_get_untracked().flatten() {
                el.set_scroll_top(el.scroll_height());
            }
        });
    };

    // Moves buffered lines into the view (unless paused).
    let flush = move || {
        let is_paused = paused.try_get_untracked().unwrap_or(true);
        let taken = pending
            .try_update_value(|p| {
                p.scheduled = false;
                (!is_paused).then(|| (std::mem::take(&mut p.lines), std::mem::take(&mut p.replace)))
            })
            .flatten();
        let Some((mut batch, replace)) = taken else {
            // Paused: say how many lines are waiting.
            if let Some(count) = pending.try_with_value(|p| p.lines.len()) {
                waiting.try_set(count);
            }
            return;
        };
        waiting.try_set(0);
        if batch.is_empty() && !replace {
            return;
        }
        lines.try_update(|lines| {
            if replace {
                cap(&mut batch);
                *lines = batch;
            } else {
                lines.append(&mut batch);
                cap(lines);
            }
        });
        if at_bottom.get_value() {
            scroll_to_end();
        }
    };
    let schedule = move || {
        let start = pending
            .try_update_value(|p| !std::mem::replace(&mut p.scheduled, true))
            .unwrap_or(false);
        if start {
            set_timeout(flush, Duration::from_millis(FLUSH_MS));
        }
    };
    let make_line = move |record: &LogRecord| {
        let id = next_id.get_value();
        next_id.set_value(id + 1);
        Line {
            id,
            level: normalize_level(record.level.as_deref()),
            record: record.clone(),
        }
    };

    use_events().on(Topic::Logs, move |event| match event {
        Event::LogBuffer(records) => {
            let batch: Vec<Line> = records.iter().map(make_line).collect();
            pending.try_update_value(|p| {
                p.lines = batch;
                p.replace = true;
            });
            received.try_set(true);
            error.try_set(None);
            schedule();
        }
        Event::Log(record) => {
            let line = make_line(record);
            pending.try_update_value(|p| {
                p.lines.push(line);
                cap(&mut p.lines);
            });
            received.try_set(true);
            schedule();
        }
        Event::Error { message, .. } => {
            error.try_set(Some(message.clone()));
        }
        _ => {}
    });

    let visible = Memo::new(move |_| {
        let min = severity(&min_level.get());
        let needle = query.with(|q| q.trim().to_lowercase());
        lines.with(|lines| {
            lines
                .iter()
                .filter(|line| severity(line.level) >= min)
                .filter(|line| {
                    needle.is_empty()
                        || [&line.record.message, &line.record.context]
                            .into_iter()
                            .flatten()
                            .any(|text| text.to_lowercase().contains(&needle))
                })
                .cloned()
                .collect::<Vec<_>>()
        })
    });
    // Keep following the end when the filter changes.
    Effect::new(move |_| {
        visible.track();
        if at_bottom.get_value() {
            scroll_to_end();
        }
    });

    let toggle_pause = move |_| {
        let resume = paused.get_untracked();
        paused.set(!resume);
        if resume {
            at_bottom.set_value(true);
            flush();
        }
    };
    let download = move |_| {
        let text: String = visible.with_untracked(|lines| {
            lines
                .iter()
                .map(|line| {
                    format!(
                        "{} {:<5} {}: {}\n",
                        line.record.timestamp.to_rfc3339(),
                        line.level.to_uppercase(),
                        line.record.context.as_deref().unwrap_or("-"),
                        line.record.message.as_deref().unwrap_or("")
                    )
                })
                .collect()
        });
        let name = format!("slskr-{}.log", chrono::Utc::now().format("%Y%m%d-%H%M%S"));
        if let Err(err) = download_file(text.as_bytes(), &name, Some("text/plain")) {
            error.set(Some(format!("Could not save the log: {err:?}")));
        }
    };

    view! {
        <Card title="Live log" flush=true class="log-card"
            description="Recent records from the daemon, newest at the bottom."
            actions=ViewFn::from(move || view! {
                <span class=move || if paused.get() { "badge badge--warning" } else { "badge badge--success" }>
                    <span class=move || if paused.get() { "status-dot" } else { "status-dot status-dot--online status-dot--pulse" }></span>
                    {move || if paused.get() {
                        let n = waiting.get();
                        if n > 0 { format!("Paused, {n} new") } else { "Paused".to_owned() }
                    } else {
                        "Live".to_owned()
                    }}
                </span>
            })>
            <div class="toolbar log-toolbar">
                <label class="sr-only" for="log-level">"Minimum level"</label>
                <select id="log-level" class="select select--sm select--auto log-level"
                    on:change=move |ev| min_level.set(event_target_value(&ev))
                    prop:value=move || min_level.get()>
                    <option value="trace">"All levels"</option>
                    <option value="debug">"Debug and above"</option>
                    <option value="info">"Info and above"</option>
                    <option value="warn">"Warnings and errors"</option>
                    <option value="error">"Errors only"</option>
                </select>
                <div class="input-icon grow">
                    <Icon name="search"/>
                    <input class="input input--sm log-filter" type="search" placeholder="Filter messages"
                        aria-label="Filter messages" prop:value=move || query.get()
                        on:input=move |ev| query.set(event_target_value(&ev))/>
                </div>
                <div class="btn-group">
                    <button type="button" class="btn btn--sm log-pause" aria-pressed=move || paused.get().to_string()
                        on:click=toggle_pause>
                        {move || if paused.get() {
                            view! { <Icon name="play"/>"Resume"}.into_any()
                        } else {
                            view! { <Icon name="pause"/>"Pause"}.into_any()
                        }}
                    </button>
                    <button type="button" class="btn btn--sm btn--icon" aria-label="Clear view" title="Clear view"
                        on:click=move |_| lines.set(Vec::new())>
                        <Icon name="trash-2"/>
                    </button>
                    <button type="button" class="btn btn--sm btn--icon" aria-label="Download shown lines" title="Download shown lines"
                        on:click=download>
                        <Icon name="download"/>
                    </button>
                </div>
            </div>
            {move || error.get().map(|message| view! {
                <div class="log-error"><Alert variant="danger">{message}</Alert></div>
            })}
            <div class="log-view" node_ref=view_ref role="log" aria-live="off" aria-label="Log records" tabindex="0"
                on:scroll=move |_| {
                    if let Some(el) = view_ref.get_untracked() {
                        let gap = el.scroll_height() - el.scroll_top() - el.client_height();
                        at_bottom.set_value(gap <= 24);
                    }
                }>
                <For each=move || visible.get() key=|line| line.id
                    children=move |line| view! { <LogLine line/> }/>
                {move || visible.with(Vec::is_empty).then(|| view! {
                    <p class="log-empty muted">{move || if !received.get() {
                        "Waiting for log records…"
                    } else if lines.with(Vec::is_empty) {
                        "No log records yet."
                    } else {
                        "No records match the filter."
                    }}</p>
                })}
            </div>
            <div class="log-footer muted text-xs">
                {move || {
                    let shown = visible.with(Vec::len);
                    let total = lines.with(Vec::len);
                    if shown == total {
                        format!("{total} records (up to {MAX_LINES} are kept)")
                    } else {
                        format!("{shown} of {total} records shown")
                    }
                }}
            </div>
        </Card>
    }
}

#[component]
fn LogLine(line: Line) -> impl IntoView {
    let Line { level, record, .. } = line;
    let class = format!("log-line log-line--{level}");
    let badge = format!("badge log-level-badge {}", level_badge(level));
    let context = record
        .sub_context
        .as_ref()
        .map(|sub| format!("{}/{sub}", record.context.as_deref().unwrap_or("")))
        .or(record.context.clone())
        .unwrap_or_default();
    view! {
        <div class=class>
            <time class="log-line__time" datetime=record.timestamp.to_rfc3339() title=format::full(record.timestamp)>
                {format::clock(record.timestamp)}
            </time>
            <span class=badge>{level}</span>
            <span class="log-line__context" title=context.clone()>{context.clone()}</span>
            <span class="log-line__message">{record.message.unwrap_or_default()}</span>
        </div>
    }
}

#[component]
fn LogFiles() -> impl IntoView {
    let files = RwSignal::new(None::<Vec<LogFile>>);
    let error = RwSignal::new(None::<String>);
    let revision = RwSignal::new(0_u32);
    let open_name = RwSignal::new(None::<String>);
    let contents = RwSignal::new(None::<Result<String, String>>);
    Effect::new(move |_| {
        revision.track();
        spawn_local_scoped_with_cancellation(async move {
            match api::get::<Vec<LogFile>>("/logs/files").await {
                Ok(mut list) => {
                    list.sort_by_key(|a| std::cmp::Reverse(a.modified_at));
                    files.set(Some(list));
                    error.set(None);
                }
                Err(err) => {
                    files.set(Some(Vec::new()));
                    error.set(Some(err.to_string()));
                }
            }
        });
    });
    let view_file = move |name: String| {
        contents.set(None);
        open_name.set(Some(name.clone()));
        leptos::task::spawn_local(async move {
            let result = api::get_text(&format!("/logs/files/{}", encode_uri_component(&name)))
                .await
                .map_err(|e| e.to_string());
            if open_name.get_untracked().as_deref() == Some(name.as_str()) {
                contents.try_set(Some(result));
            }
        });
    };
    let api_base = config::api_base_url();

    view! {
        <Card title="Log files" flush=true class="log-files"
            description="One file per day in the data directory's logs folder."
            actions=ViewFn::from(move || view! {
                <button type="button" class="btn btn--sm btn--icon" aria-label="Refresh log files" title="Refresh"
                    on:click=move |_| revision.update(|r| *r += 1)>
                    <Icon name="refresh-cw"/>
                </button>
            })>
            {move || error.get().map(|message| view! {
                <div class="log-error"><Alert variant="danger">{message}</Alert></div>
            })}
            {move || {
                let api_base = api_base.clone();
                match files.get() {
                    None => view! { <div class="loading" role="status"><span class="spinner"></span>"Loading…"</div> }.into_any(),
                    Some(list) if list.is_empty() => if error.with(Option::is_some) {
                        ().into_any()
                    } else {
                        view! {
                            <EmptyState icon="file-text" title="No log files"
                                description="Log files are written when logging.file is on."/>
                        }.into_any()
                    },
                    Some(list) => view! {
                        <div class="table-wrap">
                            <table class="table">
                                <thead>
                                    <tr>
                                        <th>"Name"</th>
                                        <th class="num">"Size"</th>
                                        <th class="hide-sm">"Modified"</th>
                                        <th class="actions"><span class="sr-only">"Actions"</span></th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {list.into_iter().map(|file| {
                                        let href = format!("{api_base}/logs/files/{}", encode_uri_component(&file.name));
                                        let name = file.name.clone();
                                        let open = file.name.clone();
                                        view! {
                                            <tr>
                                                <td>
                                                    <button type="button" class="link-button files-entry mono text-sm"
                                                        on:click=move |_| view_file(open.clone())>
                                                        <Icon name="file-text" class="files-icon"/>{name.clone()}
                                                    </button>
                                                </td>
                                                <td class="num muted">{format_bytes(file.size as f64)}</td>
                                                <td class="hide-sm muted nowrap" title=format::full(file.modified_at)>
                                                    {format::relative(file.modified_at)}
                                                </td>
                                                <td class="actions">
                                                    <button type="button" class="btn btn--action"
                                                        aria-label=format!("View {name}") title="View"
                                                        on:click={
                                                            let name = name.clone();
                                                            move |_| view_file(name.clone())
                                                        }>
                                                        <Icon name="eye"/>
                                                    </button>
                                                    <a class="btn btn--action" href=href download=name.clone()
                                                        aria-label=format!("Download {name}") title="Download">
                                                        <Icon name="download"/>
                                                    </a>
                                                </td>
                                            </tr>
                                        }
                                    }).collect_view()}
                                </tbody>
                            </table>
                        </div>
                    }.into_any(),
                }
            }}
        </Card>
        <Modal size="xl" title="Log file" class="log-file-modal"
            open=Signal::derive(move || open_name.with(Option::is_some))
            on_close=Callback::new(move |()| open_name.set(None))>
            {move || view! {
                <p class="mono text-sm log-file-name">{open_name.get().unwrap_or_default()}</p>
                {match contents.get() {
                    None => view! { <div class="loading" role="status"><span class="spinner"></span>"Loading…"</div> }.into_any(),
                    Some(Ok(text)) if text.is_empty() => view! { <p class="muted">"The file is empty."</p> }.into_any(),
                    Some(Ok(text)) => view! { <pre class="code code--scroll code--plain log-file-content">{text}</pre> }.into_any(),
                    Some(Err(message)) => view! { <Alert variant="danger">{message}</Alert> }.into_any(),
                }}
            }}
        </Modal>
    }
}
