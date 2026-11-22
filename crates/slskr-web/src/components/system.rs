pub mod config;
pub mod files;
pub mod logs;
pub mod shares;

use leptos::prelude::*;
use leptos_router::components::A;
use leptos_router::hooks::use_params_map;

#[component]
pub fn System() -> impl IntoView {
    let params = use_params_map();
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
    let base = base.trim_end_matches('/');
    let tabs = [
        ("info", "Info"),
        ("options", "Options"),
        ("shares", "Shares"),
        ("files", "Files"),
        ("logs", "Logs"),
        ("data", "Data"),
        ("events", "Events"),
    ];
    view! {
        <div class="system">
            <section class="ui raised segment">
                <nav class="ui top attached tabular menu" aria-label="System">
                    {tabs.into_iter().map(|(path, title)| view! {
                        <A href=format!("{base}/system/{path}") attr:class=move || if tab.get() == path { "active item" } else { "item" }>{title}</A>
                    }).collect_view()}
                </nav>
                <div class="ui bottom attached segment">
                    {move || match tab.read().as_str() {
                        "options" => view! { <crate::components::system::config::Config/> }.into_any(),
                        "shares" => view! { <crate::components::system::shares::Shares/> }.into_any(),
                        "files" => view! { <crate::components::system::files::Files/> }.into_any(),
                        "logs" => view! { <crate::components::system::logs::Logs/> }.into_any(),
                        "data" => view! { <SystemData/> }.into_any(),
                        "events" => view! { <SystemEvents/> }.into_any(),
                        _ => view! { <SystemInfo/> }.into_any(),
                    }}
                </div>
            </section>
        </div>
    }
}

#[component]
fn SystemInfo() -> impl IntoView {
    let state = use_context::<crate::components::app::AppContext>()
        .map(|c| c.state)
        .unwrap_or_default();
    let message = RwSignal::new(String::new());
    let restarting = RwSignal::new(false);
    let shutdown = RwSignal::new(false);
    let restart = move |_: web_sys::MouseEvent| {
        restarting.set(true);
        let message = message;
        leptos::task::spawn_local(async move {
            match crate::services::api::put_empty::<serde_json::Value>("/application").await {
                Ok(_) => message.set("Restart requested".into()),
                Err(e) => message.set(e.to_string()),
            }
            restarting.set(false);
        });
    };
    let stop = move |_: web_sys::MouseEvent| {
        shutdown.set(true);
        let message = message;
        leptos::task::spawn_local(async move {
            match crate::services::api::delete::<serde_json::Value>("/application").await {
                Ok(_) => message.set("Shutdown requested".into()),
                Err(e) => message.set(e.to_string()),
            }
            shutdown.set(false);
        });
    };
    view! { <div class="header-buttons"><button class="ui primary button" disabled=restarting on:click=restart>"Restart"</button><button class="ui negative button" disabled=shutdown on:click=stop>"Shutdown"</button><p role="status">{move || message.get()}</p></div><pre class="ui segment">{move || serde_json::to_string_pretty(&state.get()).unwrap_or_else(|e| e.to_string())}</pre> }
}

#[component]
fn SystemData() -> impl IntoView {
    let status = RwSignal::new(String::new());
    let clear = move |direction: &'static str| {
        let status = status;
        let path = format!("/transfers/{direction}s/all/completed");
        leptos::task::spawn_local(async move {
            match crate::services::api::delete::<serde_json::Value>(&path).await {
                Ok(_) => status.set(format!("Completed {direction}s cleared")),
                Err(e) => status.set(e.to_string()),
            }
        });
    };
    view! { <h3 class="ui header">"Transfer Data"</h3><hr/><p>"Remove completed transfers if the Uploads or Downloads pages become unresponsive."</p><button class="ui primary button" on:click=move |_: web_sys::MouseEvent| clear("upload")>"Clear All Completed Uploads"</button><button class="ui primary button" on:click=move |_: web_sys::MouseEvent| clear("download")>"Clear All Completed Downloads"</button><p role="status">{move || status.get()}</p> }
}

#[component]
fn SystemEvents() -> impl IntoView {
    let events = RwSignal::new(Vec::<slskr_api::events::EventRecord>::new());
    let error = RwSignal::new(String::new());
    let page = RwSignal::new(0usize);
    Effect::new(move |_| {
        let page = page.get();
        leptos::task::spawn_local(async move {
            match crate::services::api::get::<Vec<slskr_api::events::EventRecord>>(&format!(
                "/events?offset={}&limit=10",
                page * 10
            ))
            .await
            {
                Ok(data) => events.set(data),
                Err(e) => error.set(e.to_string()),
            }
        });
    });
    view! { <div class="header-buttons"><button class="ui button" on:click=move |_: web_sys::MouseEvent| page.update(|p| *p = p.saturating_sub(1))>"Previous"</button><span>{move || format!("Page {}", page.get() + 1)}</span><button class="ui button" on:click=move |_: web_sys::MouseEvent| page.update(|p| *p += 1)>"Next"</button></div><p role="alert">{move || error.get()}</p><ul class="ui divided list">{move || events.get().into_iter().map(|e| view! { <li class="item"><strong>{format!("{:?}", e.kind)}</strong>{" "}{e.timestamp.to_string()}<pre>{e.data.unwrap_or_default()}</pre></li> }).collect_view()}</ul> }
}
