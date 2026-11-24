//! System → Events: the recorded event history (`GET /api/events/history`), newest first.

use std::collections::HashSet;

use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use serde_json::Value;
use slskr_types::events::{Event, EventType};

use super::format;
use crate::{
    services::{api, util::get_file_name},
    ui::{Alert, Card, EmptyState, Icon},
};

const PAGE_SIZE: usize = 25;

/// Badge variant, icon and label of an event type.
fn kind_badge(kind: EventType) -> (&'static str, &'static str, &'static str) {
    match kind {
        EventType::DownloadFileComplete => ("success", "download", "Download complete"),
        EventType::DownloadDirectoryComplete => ("success", "folder", "Folder downloaded"),
        EventType::UploadFileComplete => ("primary", "upload", "Upload complete"),
        EventType::PrivateMessageReceived => ("purple", "message-square", "Private message"),
        EventType::RoomMessageReceived => ("purple", "messages-square", "Room message"),
        EventType::ServerConnected => ("success", "plug-zap", "Connected"),
        EventType::ServerDisconnected => ("warning", "unplug", "Disconnected"),
    }
}

fn text<'a>(data: &'a Value, pointer: &str) -> &'a str {
    data.pointer(pointer).and_then(Value::as_str).unwrap_or("")
}

/// A one-line description of the event's data.
fn summary(event: &Event) -> String {
    let data = &event.data;
    match event.kind {
        EventType::DownloadFileComplete | EventType::UploadFileComplete => {
            let file = get_file_name(text(data, "/remote_filename")).to_owned();
            let user = text(data, "/transfer/username");
            let direction = if event.kind == EventType::DownloadFileComplete {
                "from"
            } else {
                "to"
            };
            if user.is_empty() {
                file
            } else {
                format!("{file} {direction} {user}")
            }
        }
        EventType::DownloadDirectoryComplete => format!(
            "{} from {}",
            get_file_name(text(data, "/remote_directory")),
            text(data, "/username")
        ),
        EventType::PrivateMessageReceived => {
            format!("{}: {}", text(data, "/username"), text(data, "/message"))
        }
        EventType::RoomMessageReceived => format!(
            "{} in {}: {}",
            text(data, "/username"),
            text(data, "/room"),
            text(data, "/message")
        ),
        EventType::ServerConnected => "Logged in to the Soulseek server".to_owned(),
        EventType::ServerDisconnected => {
            let reason = [text(data, "/message"), text(data, "/error")]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join(": ");
            if reason.is_empty() {
                "Disconnected from the Soulseek server".to_owned()
            } else {
                reason
            }
        }
    }
}

#[component]
pub fn Events() -> impl IntoView {
    let page = RwSignal::new(0_usize);
    let revision = RwSignal::new(0_u32);
    let events = RwSignal::new(None::<Vec<Event>>);
    let has_next = RwSignal::new(false);
    let loading = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let expanded = RwSignal::new(HashSet::<String>::new());

    Effect::new(move |_| {
        let offset = page.get() * PAGE_SIZE;
        revision.track();
        loading.set(true);
        error.set(None);
        spawn_local_scoped_with_cancellation(async move {
            // One extra row says whether there is a next page.
            let path = format!("/events/history?offset={offset}&limit={}", PAGE_SIZE + 1);
            match api::get::<Vec<Event>>(&path).await {
                Ok(mut list) => {
                    has_next.set(list.len() > PAGE_SIZE);
                    list.truncate(PAGE_SIZE);
                    events.set(Some(list));
                }
                Err(err) => {
                    error.set(Some(err.to_string()));
                    events.update(|e| {
                        e.get_or_insert_with(Vec::new);
                    });
                }
            }
            loading.set(false);
        });
    });

    let range = move || {
        let count = events.with(|e| e.as_ref().map_or(0, Vec::len));
        let start = page.get() * PAGE_SIZE;
        if count == 0 {
            format!("Page {}", page.get() + 1)
        } else {
            format!("{}–{}", start + 1, start + count)
        }
    };

    view! {
        <Card title="Event history" flush=true class="events-card"
            description="Completed transfers, messages and server connection changes, newest first."
            actions=ViewFn::from(move || view! {
                <button type="button" class="btn btn--sm btn--icon" aria-label="Refresh events" title="Refresh"
                    disabled=move || loading.get() on:click=move |_| revision.update(|r| *r += 1)>
                    <Icon name="refresh-cw" spin=Signal::derive(move || loading.get())/>
                </button>
            })
            footer=ViewFn::from(move || view! {
                <span class="muted text-sm tabular events-range">{range}</span>
                {move || loading.get().then(|| view! { <span class="spinner muted" role="status" aria-label="Loading"></span> })}
                <div class="btn-group ml-auto">
                    <button type="button" class="btn btn--sm events-prev"
                        disabled=move || page.get() == 0 || loading.get()
                        on:click=move |_| page.update(|p| *p = p.saturating_sub(1))>
                        <Icon name="chevron-left"/>"Previous"
                    </button>
                    <button type="button" class="btn btn--sm events-next"
                        disabled=move || !has_next.get() || loading.get()
                        on:click=move |_| page.update(|p| *p += 1)>
                        "Next"<Icon name="chevron-right"/>
                    </button>
                </div>
            })>
            {move || error.get().map(|message| view! {
                <div class="events-error"><Alert variant="danger">{message}</Alert></div>
            })}
            {move || match events.get() {
                None => view! { <div class="loading" role="status"><span class="spinner"></span>"Loading events…"</div> }.into_any(),
                Some(list) if list.is_empty() => if error.with(Option::is_some) {
                    ().into_any()
                } else {
                    view! {
                        <EmptyState icon="history" title="No events recorded"
                            description="Finished downloads and uploads, received messages and server connection changes appear here."/>
                    }.into_any()
                },
                Some(list) => view! {
                    <div class="table-wrap" class:is-loading=move || loading.get()
                        aria-busy=move || loading.get().then_some("true")>
                        <table class="table events-table">
                            <thead>
                                <tr>
                                    <th class="shrink"><span class="sr-only">"Details"</span></th>
                                    <th class="shrink">"Type"</th>
                                    <th>"Summary"</th>
                                    <th class="shrink">"When"</th>
                                </tr>
                            </thead>
                            <tbody>
                                {list.into_iter().map(|event| view! { <EventRow event expanded/> }).collect_view()}
                            </tbody>
                        </table>
                    </div>
                }.into_any(),
            }}
        </Card>
    }
}

#[component]
fn EventRow(event: Event, expanded: RwSignal<HashSet<String>>) -> impl IntoView {
    let id = event.id.clone();
    let detail_id = format!("event-{id}");
    let open = {
        let id = id.clone();
        Memo::new(move |_| expanded.with(|e| e.contains(&id)))
    };
    let toggle = move |_| {
        let id = id.clone();
        expanded.update(move |e| {
            if !e.remove(&id) {
                e.insert(id);
            }
        });
    };
    let (variant, icon, label) = kind_badge(event.kind);
    let summary = summary(&event);
    let json = serde_json::to_string_pretty(&event.data).unwrap_or_default();
    let detail_target = detail_id.clone();
    view! {
        <tr class="event-row" class:is-selected=move || open.get()>
            <td class="shrink">
                <button type="button" class="btn btn--action event-toggle"
                    aria-expanded=move || open.get().to_string() aria-controls=detail_target
                    aria-label=move || if open.get() { "Hide details" } else { "Show details" }
                    title=move || if open.get() { "Hide details" } else { "Show details" }
                    on:click=toggle>
                    <Icon name=Signal::derive(move || if open.get() { "chevron-down" } else { "chevron-right" }.to_owned())/>
                </button>
            </td>
            <td class="shrink">
                <span class=format!("badge badge--{variant}") title=event.kind.as_str()>
                    <Icon name=icon/>{label}
                </span>
            </td>
            <td class="event-summary"><span class="truncate" title=summary.clone()>{summary.clone()}</span></td>
            <td class="shrink muted" title=format::full(event.timestamp)>{format::relative(event.timestamp)}</td>
        </tr>
        <tr class="event-detail" id=detail_id hidden=move || !open.get()>
            <td colspan="4">
                <dl class="dl text-sm event-meta">
                    <dt>"Type"</dt><dd class="mono">{event.kind.as_str()}</dd>
                    <dt>"Time"</dt><dd>{format::full(event.timestamp)}</dd>
                    <dt>"ID"</dt><dd class="mono">{event.id.clone()}</dd>
                </dl>
                <pre class="code code--plain event-json">{json}</pre>
            </td>
        </tr>
    }
}
