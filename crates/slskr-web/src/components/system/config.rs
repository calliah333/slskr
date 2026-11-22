use leptos::prelude::*;

#[component]
pub fn Config() -> impl IntoView {
    let context = use_context::<crate::components::app::AppContext>();
    let options = context.map(|c| c.options).unwrap_or_default();
    let remote = move || options.get().remote_configuration;
    let yaml = RwSignal::new(String::new());
    let status = RwSignal::new(String::new());
    let load = move |_| {
        leptos::task::spawn_local(async move {
            match crate::services::api::get_text("/options/yaml").await {
                Ok(text) => yaml.set(text),
                Err(e) => status.set(e.to_string()),
            }
        });
    };
    let validate = move |_| {
        let content = yaml.get();
        leptos::task::spawn_local(async move {
            match crate::services::api::post_text("/options/yaml/validate", &content).await {
                Ok(text) => status.set(if text.is_empty() {
                    "Configuration is valid".into()
                } else {
                    text
                }),
                Err(e) => status.set(e.to_string()),
            }
        });
    };
    let save = move |_| {
        let content = yaml.get();
        leptos::task::spawn_local(async move {
            match crate::services::api::put_text("/options/yaml", &content).await {
                Ok(_) => status.set("Configuration saved".into()),
                Err(e) => status.set(e.to_string()),
            }
        });
    };
    view! { <div class="header-buttons"><button class="ui button" on:click=load>"Reload YAML"</button><button class="ui button" disabled=move || !remote() on:click=validate>"Validate"</button><button class="ui primary button" disabled=move || !remote() on:click=save>"Save options"</button></div><textarea class="ui segment" aria-label="Options YAML" rows="24" prop:value=move || yaml.get() on:input=move |ev| yaml.set(event_target_value(&ev))></textarea><p role="status">{move || status.get()}</p><Show when=move || !remote()><p class="ui warning message">"Remote administration is disabled by configuration."</p></Show> }
}
