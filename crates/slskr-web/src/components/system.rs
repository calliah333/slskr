//! The System page (`/system/:tab`): daemon status, shares, the download directories,
//! logs, the event history and maintenance. Settings have their own page (`/settings`).

pub mod events;
pub mod files;
mod format;
pub mod info;
pub mod logs;
pub mod maintenance;
pub mod settings;
pub mod shares;

use super::app::RouteRedirect;
use leptos::prelude::*;
use leptos_router::components::A;
use leptos_router::hooks::{use_location, use_params_map};

use crate::ui::{Icon, PageHeader};

/// `(path, label, icon)` of each tab, in display order.
const TABS: [(&str, &str, &str); 6] = [
    ("info", "Info", "info"),
    ("shares", "Shares", "share-2"),
    ("files", "Files", "folder"),
    ("logs", "Logs", "file-text"),
    ("events", "Events", "history"),
    ("maintenance", "Maintenance", "hard-drive"),
];

#[component]
pub fn System() -> impl IntoView {
    let params = use_params_map();
    let location = use_location();
    let tab = Memo::new(move |_| {
        params.with(|params| {
            if params.get("root").is_some() {
                "files".to_owned()
            } else {
                params.get("tab").unwrap_or_else(|| "info".to_owned())
            }
        })
    });
    let base = crate::config::url_base();
    let base = base.trim_end_matches('/').to_owned();
    view! {
        <div class="system stack">
            <PageHeader title="System"/>
            <nav class="tabs-underline system-tabs" aria-label="System">
                {TABS.into_iter().map(|(path, title, icon)| {
                    let active = move || tab.get() == path;
                    view! {
                        <A href=format!("{base}/system/{path}")
                            attr:class=move || if active() { "tabs__tab is-active" } else { "tabs__tab" }
                            attr:aria-current=move || active().then_some("page")>
                            <Icon name=icon/>
                            {title}
                        </A>
                    }
                }).collect_view()}
            </nav>
            {move || match tab.read().as_str() {
                // Settings moved to their own page; "options" is the tab's former name.
                "settings" | "options" => {
                    let search = location.search.get_untracked();
                    let path = if search.is_empty() {
                        "/settings".to_owned()
                    } else {
                        format!("/settings?{}", search.trim_start_matches('?'))
                    };
                    view! { <RouteRedirect path/> }.into_any()
                }
                "data" => view! { <RouteRedirect path="/system/maintenance"/> }.into_any(),
                "shares" => view! { <shares::Shares/> }.into_any(),
                "files" => view! { <files::Files/> }.into_any(),
                "logs" => view! { <logs::Logs/> }.into_any(),
                "events" => view! { <events::Events/> }.into_any(),
                "maintenance" => view! { <maintenance::Maintenance/> }.into_any(),
                _ => view! { <info::Info/> }.into_any(),
            }}
        </div>
    }
}
