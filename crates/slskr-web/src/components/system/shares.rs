use leptos::prelude::*;

#[component]
pub fn Shares() -> impl IntoView {
    let shares = RwSignal::new(Vec::<slskr_api::shares::Share>::new());
    let error = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let refresh = move || {
        busy.set(true);
        leptos::task::spawn_local(async move {
            match crate::services::api::get::<slskr_api::shares::SharesByHost>("/shares").await {
                Ok(result) => shares.set(result.into_values().flatten().collect()),
                Err(e) => error.set(e.to_string()),
            }
            busy.set(false);
        });
    };
    let scan = move || {
        busy.set(true);
        leptos::task::spawn_local(async move {
            match crate::services::api::put_empty::<serde_json::Value>("/shares").await {
                Ok(_) => error.set("Share scan started".into()),
                Err(e) => error.set(e.to_string()),
            }
            busy.set(false);
        });
    };
    let cancel = move || {
        busy.set(true);
        leptos::task::spawn_local(async move {
            match crate::services::api::delete::<serde_json::Value>("/shares").await {
                Ok(_) => error.set("Share scan cancelled".into()),
                Err(e) => error.set(e.to_string()),
            }
            busy.set(false);
        });
    };
    Effect::new(move |_| refresh());
    view! { <div class="header-buttons"><button class="ui primary button" disabled=busy on:click=move |_: web_sys::MouseEvent| scan()>"Rescan Shares"</button><button class="ui button" disabled=busy on:click=move |_: web_sys::MouseEvent| cancel()>"Cancel Scan"</button><button class="ui button" disabled=busy on:click=move |_: web_sys::MouseEvent| refresh()>"Refresh"</button></div><p role="status">{move || error.get()}</p><table class="ui celled table"><thead><tr><th>"Share"</th><th>"Local path"</th><th>"Directories"</th><th>"Files"</th></tr></thead><tbody>{move || shares.get().into_iter().map(|s| view! { <tr><td>{s.alias.unwrap_or(s.raw)}</td><td>{s.local_path.unwrap_or_default()}</td><td>{s.directories.map(|n| n.to_string()).unwrap_or_default()}</td><td>{s.files.map(|n| n.to_string()).unwrap_or_default()}</td></tr> }).collect_view()}</tbody></table> }
}
