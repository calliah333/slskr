use leptos::prelude::*;

/// Raw editor for `slskr.toml`. The daemon validates before writing, keeps a `.bak` of the
/// previous file, and writes the text as given, so comments survive.
#[component]
pub fn Config() -> impl IntoView {
    let toml = RwSignal::new(String::new());
    // The text most recently loaded from (or saved to) the daemon. `None` until a load
    // succeeds, so an editor that never loaded can not be written over `slskr.toml`.
    let loaded = RwSignal::new(None::<String>);
    let status = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let load = move || {
        leptos::task::spawn_local(async move {
            match crate::services::api::get_text("/options/toml").await {
                Ok(text) => {
                    let _ = toml.try_set(text.clone());
                    let _ = loaded.try_set(Some(text));
                }
                Err(e) => {
                    let _ = status.try_set(e.to_string());
                }
            }
        });
    };
    load();
    let dirty = move || {
        loaded.with(|loaded| {
            loaded
                .as_ref()
                .is_some_and(|loaded| toml.with(|toml| toml != loaded))
        })
    };
    let can_save = move || !busy.get() && dirty() && !toml.with(|t| t.trim().is_empty());
    let validate = move |_| {
        let content = toml.get();
        leptos::task::spawn_local(async move {
            match crate::services::api::post_text("/options/toml/validate", &content).await {
                Ok(text) => {
                    let _ = status.try_set(if text.is_empty() {
                        "Configuration is valid".into()
                    } else {
                        text
                    });
                }
                Err(e) => {
                    let _ = status.try_set(e.to_string());
                }
            }
        });
    };
    let save = move |_| {
        if !can_save() {
            return;
        }
        let content = toml.get();
        if content.trim().is_empty() {
            status.set("Refusing to save an empty configuration".into());
            return;
        }
        busy.set(true);
        leptos::task::spawn_local(async move {
            let result = crate::services::api::put_text("/options/toml", &content).await;
            let _ = busy.try_set(false);
            match result {
                Ok(_) => {
                    let _ = loaded.try_set(Some(content));
                    let _ = status.try_set("Configuration saved".into());
                }
                Err(e) => {
                    let _ = status.try_set(e.to_string());
                }
            }
        });
    };
    view! { <div class="header-buttons"><button class="ui button" on:click=move |_| load()>"Reload TOML"</button><button class="ui button" disabled=move || loaded.with(Option::is_none) on:click=validate>"Validate"</button><button class="ui primary button" disabled=move || !can_save() on:click=save>"Save options"</button></div><textarea class="ui segment" aria-label="Options TOML" rows="24" prop:value=move || toml.get() on:input=move |ev| toml.set(event_target_value(&ev))></textarea><p role="status">{move || status.get()}</p> }
}
