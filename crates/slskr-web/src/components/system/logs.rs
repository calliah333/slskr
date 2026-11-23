use leptos::prelude::*;

#[component]
pub fn Logs() -> impl IntoView {
    let live = RwSignal::new(true);
    view! { <nav class="ui secondary menu"><button class="ui button" class:active=move || live.get() on:click=move |_: web_sys::MouseEvent| live.set(true)>"Live"</button><button class="ui button" class:active=move || !live.get() on:click=move |_: web_sys::MouseEvent| live.set(false)>"History"</button></nav>{move || if live.get() { view! { <LiveLogs/> }.into_any() } else { view! { <LogHistory/> }.into_any() }} }
}

#[component]
fn LiveLogs() -> impl IntoView {
    use crate::services::events::{Event, Topic, use_events};
    let lines = RwSignal::new(Vec::<slskr_types::logs::LogRecord>::new());
    let error = RwSignal::new(String::new());
    use_events().on(Topic::Logs, move |event| match event {
        Event::LogBuffer(records) => {
            lines.try_set(records.clone());
            error.try_set(String::new());
        }
        Event::Log(record) => {
            lines.try_update(|items| {
                items.push(record.clone());
                if items.len() > 500 {
                    items.drain(..items.len() - 500);
                }
            });
        }
        Event::Error { message, .. } => {
            error.try_set(message.clone());
        }
        _ => {}
    });
    view! { <p role="alert">{move || error.get()}</p><div class="ui segment" style="overflow:auto;max-height:70vh"><For each=move || lines.get() key=|line| format!("{}-{}", line.timestamp, line.message.clone().unwrap_or_default()) children=move |line| view! { <div><time>{line.timestamp.to_string()}</time>{" ["}{line.level.unwrap_or_default()}{"] "}{line.context.unwrap_or_default()}{": "}{line.message.unwrap_or_default()}</div> }/></div> }
}

#[component]
fn LogHistory() -> impl IntoView {
    let files = RwSignal::new(Vec::<slskr_types::logs::LogFile>::new());
    let selected = RwSignal::new(String::new());
    let contents = RwSignal::new(String::new());
    let error = RwSignal::new(String::new());
    Effect::new(move |_| {
        leptos::task::spawn_local(async move {
            match crate::services::api::get::<Vec<slskr_types::logs::LogFile>>("/logs/files").await
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
