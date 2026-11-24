//! System → Shares: scan state, the configured shared folders and rescanning.

use leptos::prelude::*;
use slskr_types::{application::ShareState, shares::Share};

use crate::{
    components::app::AppContext,
    config,
    services::api,
    ui::{Alert, Badge, Card, EmptyState, Icon, Progress, Stat},
};

/// Badge variant and text for the scan as a whole.
fn scan_status(scan: &ShareState) -> (&'static str, String) {
    if scan.scanning {
        (
            "primary",
            format!("Scanning {:.0}%", scan.scan_progress * 100.0),
        )
    } else if scan.faulted {
        ("danger", "Scan failed".to_owned())
    } else if scan.cancelled {
        ("warning", "Scan cancelled".to_owned())
    } else if scan.scan_pending {
        ("warning", "Rescan pending".to_owned())
    } else if scan.ready {
        ("success", "Ready".to_owned())
    } else {
        ("neutral", "Not scanned".to_owned())
    }
}

#[component]
pub fn Shares() -> impl IntoView {
    let state = use_context::<AppContext>()
        .map(|c| c.state)
        .unwrap_or_default();
    let scan = Memo::new(move |_| state.with(|s| s.shares.clone()));
    let scanning = Memo::new(move |_| scan.with(|s| s.scanning));
    let shares = RwSignal::new(None::<Vec<Share>>);
    let message = RwSignal::new(None::<(&'static str, String)>);
    let busy = RwSignal::new(false);
    let loading = RwSignal::new(false);

    let refresh = move || {
        loading.set(true);
        leptos::task::spawn_local(async move {
            match api::get::<Vec<Share>>("/shares").await {
                Ok(result) => {
                    shares.try_set(Some(result));
                }
                Err(err) => {
                    message.try_set(Some(("danger", err.to_string())));
                }
            }
            loading.try_set(false);
        });
    };
    let request = move |start: bool| {
        busy.set(true);
        message.set(None);
        leptos::task::spawn_local(async move {
            let result = if start {
                api::post_empty::<serde_json::Value>("/shares/scan").await
            } else {
                api::delete::<serde_json::Value>("/shares/scan").await
            };
            let text = match (&result, start) {
                (Ok(_), true) => ("info", "Share scan started.".to_owned()),
                (Ok(_), false) => ("warning", "Share scan cancelled.".to_owned()),
                (Err(err), _) => ("danger", err.to_string()),
            };
            message.try_set(Some(text));
            busy.try_set(false);
        });
    };
    refresh();
    // Counts change when a scan ends: reload the list then.
    Effect::new(move |was_scanning: Option<bool>| {
        let now = scanning.get();
        if was_scanning == Some(true) && !now {
            refresh();
        }
        now
    });

    let base = config::url_base();
    let settings_href = format!("{}/settings?section=shares", base.trim_end_matches('/'));
    let settings_href_empty = settings_href.clone();
    let shared = Memo::new(move |_| {
        shares.with(|s| {
            s.as_ref()
                .map_or(0, |s| s.iter().filter(|s| !s.is_excluded).count())
        })
    });

    view! {
        <div class="system-shares stack">
            <div class="stats live-stats">
                <Stat label="Shares" value=Signal::derive(move || shared.get().to_string())/>
                <Stat label="Folders" value=Signal::derive(move || scan.with(|s| s.directories.to_string()))/>
                <Stat label="Files" value=Signal::derive(move || scan.with(|s| s.files.to_string()))/>
                <div class="stat">
                    <div class="stat__label">"Scan"</div>
                    <div class="stat__value">
                        {move || {
                            let (variant, text) = scan.with(scan_status);
                            view! { <span class=format!("badge badge--{variant} shares-scan-status")>{text}</span> }
                        }}
                    </div>
                    {move || scanning.get().then(|| view! {
                        <Progress value=Signal::derive(move || scan.with(|s| s.scan_progress * 100.0))
                            label="Scan progress"/>
                    })}
                </div>
            </div>
            <div role="status" class="shares-message">
                {move || message.get().map(|(variant, text)| view! {
                    <Alert variant=variant role="none" on_dismiss=Callback::new(move |()| message.set(None))>{text}</Alert>
                })}
            </div>
            <Card title="Shared folders" flush=true
                description="Folders peers can browse and download from. Scans run at startup and after changes."
                actions=ViewFn::from(move || {
                    let settings_href = settings_href.clone();
                    view! {
                        <a class="btn btn--sm" href=settings_href><Icon name="pencil"/>"Edit folders"</a>
                        <button type="button" class="btn btn--sm btn--icon" aria-label="Refresh" title="Refresh"
                            disabled=move || loading.get() on:click=move |_| refresh()>
                            <Icon name="refresh-cw" spin=Signal::derive(move || loading.get())/>
                        </button>
                        {move || if scanning.get() {
                            view! {
                                <button type="button" class="btn btn--sm btn--danger-outline" disabled=move || busy.get()
                                    on:click=move |_| request(false)>
                                    <Icon name="circle-stop"/>"Cancel scan"
                                </button>
                            }.into_any()
                        } else {
                            view! {
                                <button type="button" class="btn btn--sm btn--primary shares-rescan" disabled=move || busy.get()
                                    aria-busy=move || busy.get().then_some("true")
                                    on:click=move |_| request(true)>
                                    {move || if busy.get() {
                                        view! { <span class="spinner" aria-hidden="true"></span> }.into_any()
                                    } else {
                                        view! { <Icon name="rotate-cw"/> }.into_any()
                                    }}
                                    "Rescan shares"
                                </button>
                            }.into_any()
                        }}
                    }
                })>
                {move || match shares.get() {
                    None => view! { <div class="loading" role="status"><span class="spinner"></span>"Loading shares…"</div> }.into_any(),
                    Some(list) if list.is_empty() => {
                        let href = settings_href_empty.clone();
                        view! {
                            <EmptyState icon="folder-open" title="No shared folders"
                                description="Share a folder so other Soulseek users can browse and download from you.">
                                <a class="btn btn--primary" href=href><Icon name="plus"/>"Add shared folders"</a>
                            </EmptyState>
                        }.into_any()
                    }
                    Some(list) => view! {
                        <div class="table-wrap">
                            <table class="table shares-table">
                                <thead>
                                    <tr>
                                        <th>"Name"</th>
                                        <th>"Local path"</th>
                                        <th class="num">"Folders"</th>
                                        <th class="num">"Files"</th>
                                        <th class="shrink">"Status"</th>
                                    </tr>
                                </thead>
                                <tbody>
                                    {list.into_iter().map(|share| {
                                        let name = share.alias.clone().unwrap_or_else(|| share.raw.clone());
                                        let local = share.local_path.clone().unwrap_or_else(|| share.raw.clone());
                                        let local_title = local.clone();
                                        let remote = share.remote_path.clone();
                                        let count = |n: Option<u32>| n.map_or_else(|| "—".to_owned(), |n| n.to_string());
                                        let excluded = share.is_excluded;
                                        let scanned = share.files.is_some();
                                        view! {
                                            <tr>
                                                <td>
                                                    <span class="row">
                                                        <Icon name=if excluded { "ban" } else { "folder" } class="muted"/>
                                                        <span class="font-medium">{name}</span>
                                                    </span>
                                                    {remote.map(|remote| view! {
                                                        <div class="muted text-xs mono shares-remote" title="The path peers see">{remote}</div>
                                                    })}
                                                </td>
                                                <td class="mono text-sm shares-path" title=local_title>{local}</td>
                                                <td class="num">{count(share.directories)}</td>
                                                <td class="num">{count(share.files)}</td>
                                                <td class="shrink">
                                                    {move || if excluded {
                                                        view! { <Badge variant="outline" text="Excluded"/> }.into_any()
                                                    } else if scanning.get() {
                                                        view! { <Badge variant="primary" text="Scanning"/> }.into_any()
                                                    } else if scanned {
                                                        view! { <Badge variant="success" text="Shared"/> }.into_any()
                                                    } else {
                                                        view! { <Badge text="Not scanned"/> }.into_any()
                                                    }}
                                                </td>
                                            </tr>
                                        }
                                    }).collect_view()}
                                </tbody>
                            </table>
                        </div>
                    }.into_any(),
                }}
            </Card>
        </div>
    }
}
