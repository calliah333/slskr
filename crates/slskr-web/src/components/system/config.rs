use leptos::prelude::*;
use slskr_api::settings::Settings;

/// Read-only view of `GET /settings` until the settings form lands: the merged values
/// (secrets left out) and the keys locked by `slskr.toml`, the environment or the
/// command line.
#[component]
pub fn Config() -> impl IntoView {
    let settings = RwSignal::new(None::<Settings>);
    let status = RwSignal::new(String::new());
    let load = move || {
        leptos::task::spawn_local(async move {
            match crate::services::api::get::<Settings>("/settings").await {
                Ok(loaded) => {
                    let _ = settings.try_set(Some(loaded));
                    let _ = status.try_set(String::new());
                }
                Err(e) => {
                    let _ = status.try_set(e.to_string());
                }
            }
        });
    };
    load();
    let values = move || {
        settings.with(|s| {
            s.as_ref()
                .and_then(|s| serde_json::to_string_pretty(&s.values.public_json()).ok())
                .unwrap_or_default()
        })
    };
    let list = move |pick: fn(&Settings) -> Vec<String>| {
        settings.with(|s| {
            let items = s.as_ref().map(pick).unwrap_or_default();
            if items.is_empty() {
                "none".to_owned()
            } else {
                items.join(", ")
            }
        })
    };
    let pending = move || settings.with(|s| s.as_ref().is_some_and(|s| s.pending_restart));
    view! {
        <div class="header-buttons"><button class="ui button" on:click=move |_| load()>"Reload"</button></div>
        <p role="status">{move || status.get()}</p>
        <Show when=pending><p class="ui warning message">"A restart is pending for some changes."</p></Show>
        <p><strong>"Locked by slskr.toml or the environment: "</strong>{move || list(|s| s.pinned.clone())}</p>
        <p><strong>"Secrets set: "</strong>{move || list(|s| s.secrets_set.clone())}</p>
        <p><strong>"API keys: "</strong>{move || list(|s| s.api_keys.iter().map(|k| format!("{} ({:?}{})", k.name, k.role, if k.pinned { ", slskr.toml" } else { "" })).collect())}</p>
        <pre class="ui segment" aria-label="Settings" style="overflow:auto;max-height:70vh">{values}</pre>
    }
}
