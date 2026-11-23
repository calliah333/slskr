use leptos::prelude::*;

#[component]
pub fn Config() -> impl IntoView {
    let context = use_context::<crate::components::app::AppContext>();
    let options = context.map(|c| c.options).unwrap_or_default();
    let remote = move || options.get().remote_configuration;
    let yaml = RwSignal::new(String::new());
    // The text most recently loaded from (or saved to) the daemon. `None` until a load
    // succeeds, so an empty editor can never be written over `slskd.yml`.
    let loaded = RwSignal::new(None::<String>);
    let status = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let load = move || {
        leptos::task::spawn_local(async move {
            match crate::services::api::get_text("/options/yaml").await {
                Ok(text) => {
                    let _ = yaml.try_set(text.clone());
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
                .is_some_and(|loaded| yaml.with(|yaml| yaml != loaded))
        })
    };
    let can_save =
        move || remote() && !busy.get() && dirty() && !yaml.with(|y| y.trim().is_empty());
    let validate = move |_| {
        let content = yaml.get();
        leptos::task::spawn_local(async move {
            match crate::services::api::post_text("/options/yaml/validate", &content).await {
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
        let content = yaml.get();
        if content.trim().is_empty() {
            status.set("Refusing to save an empty configuration".into());
            return;
        }
        busy.set(true);
        leptos::task::spawn_local(async move {
            let result = crate::services::api::put_text("/options/yaml", &content).await;
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
    view! { <div class="header-buttons"><button class="ui button" on:click=move |_| load()>"Reload YAML"</button><button class="ui button" disabled=move || !remote() || loaded.with(Option::is_none) on:click=validate>"Validate"</button><button class="ui primary button" disabled=move || !can_save() on:click=save>"Save options"</button></div><textarea class="ui segment" aria-label="Options YAML" rows="24" prop:value=move || yaml.get() on:input=move |ev| yaml.set(event_target_value(&ev))></textarea><p role="status">{move || status.get()}</p><Show when=move || !remote()><p class="ui warning message">"Remote administration is disabled by configuration."</p></Show> }
}
