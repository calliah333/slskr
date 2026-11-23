use leptos::prelude::*;

#[component]
pub fn Logs() -> impl IntoView {
    let live = RwSignal::new(true);
    view! { <nav class="ui secondary menu"><button class="ui button" class:active=move || live.get() on:click=move |_: web_sys::MouseEvent| live.set(true)>"Live"</button><button class="ui button" class:active=move || !live.get() on:click=move |_: web_sys::MouseEvent| live.set(false)>"History"</button></nav>{move || if live.get() { view! { <LiveLogs/> }.into_any() } else { view! { <LogHistory/> }.into_any() }} }
}

#[component]
fn LiveLogs() -> impl IntoView {
    let lines = RwSignal::new(Vec::<slskr_api::logs::LogRecord>::new());
    let error = RwSignal::new(String::new());
    let connection = StoredValue::new_local(None::<crate::services::hub::HubConnection>);
    Effect::new(move |_| {
        let hub = crate::services::hub::create_logs_hub();
        let lines_ref = lines;
        hub.on(slskr_api::logs::hub_methods::BUFFER, move |args| {
            if let Some(values) = args.first().and_then(|v| {
                serde_json::from_value::<Vec<slskr_api::logs::LogRecord>>(v.clone()).ok()
            }) {
                lines_ref.set(values);
            }
        });
        let lines_ref = lines;
        hub.on(slskr_api::logs::hub_methods::LOG, move |args| {
            if let Some(value) = args
                .first()
                .and_then(|v| serde_json::from_value::<slskr_api::logs::LogRecord>(v.clone()).ok())
            {
                lines_ref.update(|items| {
                    items.push(value);
                    if items.len() > 500 {
                        items.drain(..items.len() - 500);
                    }
                });
            }
        });
        connection.set_value(Some(hub.clone()));
        leptos::task::spawn_local(async move {
            if let Err(e) = hub.start().await {
                error.set(e.to_string());
            }
        });
        on_cleanup(move || {
            if let Some(hub) = connection.get_value() {
                hub.stop();
            }
        });
    });
    view! { <p role="alert">{move || error.get()}</p><div class="ui segment" style="overflow:auto;max-height:70vh"><For each=move || lines.get() key=|line| format!("{}-{}", line.timestamp, line.message.clone().unwrap_or_default()) children=move |line| view! { <div><time>{line.timestamp.to_string()}</time>{" ["}{line.level.unwrap_or_default()}{"] "}{line.context.unwrap_or_default()}{": "}{line.message.unwrap_or_default()}</div> }/></div> }
}

#[component]
fn LogHistory() -> impl IntoView {
    let files = RwSignal::new(Vec::<slskr_api::files::FilesystemFile>::new());
    let selected = RwSignal::new(String::new());
    let contents = RwSignal::new(String::new());
    let error = RwSignal::new(String::new());
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            match crate::services::api::get::<Vec<slskr_api::files::FilesystemFile>>("/logs/files")
                .await
            {
                Ok(names) => files.set(names),
                Err(e) => error.set(e.to_string()),
            }
        })
    });
    let open = move |name: String| {
        selected.set(name.clone());
        leptos::task::spawn_local(async move {
            let encoded = js_sys::encode_uri_component(&name)
                .as_string()
                .unwrap_or_default();
            match crate::services::api::get_text(&format!("/logs/files/{encoded}")).await {
                Ok(text) => contents.set(text),
                Err(e) => error.set(e.to_string()),
            }
        });
    };
    view! { <p role="alert">{move || error.get()}</p><div class="ui grid"><aside class="four wide column"><ul>{move || files.get().into_iter().map(|file| { let name = file.name; let click = name.clone(); view! { <li><button class="ui basic button" on:click=move |_: web_sys::MouseEvent| open(click.clone())>{name}</button></li> } }).collect_view()}</ul></aside><main class="twelve wide column"><h4>{move || selected.get()}</h4><pre class="ui segment" style="overflow:auto;max-height:70vh">{move || contents.get()}</pre></main></div> }
}
