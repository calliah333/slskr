fn replace_url(url: &str) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let window_value: &wasm_bindgen::JsValue = window.as_ref();
    let Ok(history) = js_sys::Reflect::get(window_value, &"history".into()) else {
        return;
    };
    let Ok(replace) = js_sys::Reflect::get(&history, &"replaceState".into()) else {
        return;
    };
    let Ok(replace) = replace.dyn_into::<js_sys::Function>() else {
        return;
    };
    let _ = replace.call3(
        &history,
        &wasm_bindgen::JsValue::NULL,
        &"".into(),
        &url.into(),
    );
}

use wasm_bindgen::JsCast;
fn initial_username() -> String {
    let from_query = web_sys::window().and_then(|window| {
        let search = window.location().search().ok()?;
        search.trim_start_matches('?').split('&').find_map(|part| {
            let (name, value) = part.split_once('=')?;
            (name == "username").then(|| value.replace('+', " "))
        })
    });
    from_query
        .or_else(|| {
            web_sys::window()?
                .session_storage()
                .ok()
                .flatten()?
                .get_item("slskr.activeChat")
                .ok()
                .flatten()
        })
        .unwrap_or_default()
}

use leptos::prelude::*;
use leptos::task::spawn_local_scoped_with_cancellation;
use slskr_api::conversations::Conversation;
use wasm_bindgen_futures::spawn_local;

mod conversation;

#[component]
pub fn Chat() -> impl IntoView {
    let conversations = RwSignal::new(Vec::<Conversation>::new());
    let active = RwSignal::new(initial_username());
    let draft = RwSignal::new(String::new());
    let recipient = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    let refresh = move || {
        let selected = active.get_untracked();
        spawn_local(async move {
            match crate::services::api::get::<Vec<Conversation>>("/conversations").await {
                Ok(mut value) => {
                    value.retain(|conversation| conversation.username != "..");
                    let names = value.iter().map(|c| c.username.clone()).collect::<Vec<_>>();
                    let selected = if names.contains(&selected) {
                        selected
                    } else {
                        names.first().cloned().unwrap_or_default()
                    };
                    if !selected.is_empty() {
                        let path = format!(
                            "/conversations/{}",
                            crate::services::util::encode_uri_component(&selected)
                        );
                        if let Some(conversation) = value.iter().find(|c| c.username == selected) {
                            if conversation.has_un_acknowledged_messages {
                                if let Err(e) = crate::services::api::request_text(
                                    gloo_net::http::Method::PUT,
                                    &path,
                                    None,
                                )
                                .await
                                {
                                    error.set(Some(e.to_string()));
                                }
                            }
                        }
                        match crate::services::api::get::<Conversation>(&path).await {
                            Ok(full) => {
                                if let Some(item) =
                                    value.iter_mut().find(|c| c.username == selected)
                                {
                                    *item = full;
                                }
                            }
                            Err(e) => error.set(Some(e.to_string())),
                        }
                    }
                    active.set(selected);
                    conversations.set(value);
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };
    Effect::new(move |_| refresh());
    spawn_local_scoped_with_cancellation(async move {
        loop {
            crate::services::util::sleep(5_000).await;
            refresh();
        }
    });

    let select = move |username: String| {
        active.set(username.clone());
        if let Some(storage) = web_sys::window().and_then(|w| w.session_storage().ok().flatten()) {
            let _ = storage.set_item("slskr.activeChat", &username);
        }
        if let Some(window) = web_sys::window() {
            let url = format!(
                "/chat?username={}",
                crate::services::util::encode_uri_component(&username)
            );
            let _ = window.history().and_then(|h| {
                h.replace_state_with_url(&wasm_bindgen::JsValue::NULL, "", Some(&url))
            });
        }
        refresh();
    };
    let send = move |_| {
        let username = if active.get_untracked().is_empty() {
            recipient.get_untracked()
        } else {
            active.get_untracked()
        };
        let text = draft.get_untracked();
        if username.trim().is_empty() || text.trim().is_empty() || username == ".." {
            return;
        }
        draft.set(String::new());
        spawn_local(async move {
            let path = format!(
                "/conversations/{}",
                crate::services::util::encode_uri_component(&username)
            );
            match crate::services::api::post_text(&path, &text).await {
                Ok(_) => {
                    recipient.set(String::new());
                    active.set(username);
                    refresh();
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };
    let remove = move |username: String| {
        spawn_local(async move {
            let path = format!(
                "/conversations/{}",
                crate::services::util::encode_uri_component(&username)
            );
            match crate::services::api::request_text(gloo_net::http::Method::DELETE, &path, None)
                .await
            {
                Ok(_) => {
                    if active.get_untracked() == username {
                        active.set(String::new());
                    }
                    refresh();
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    view! {
        <section class="chat">
            <nav class="conversation-menu">
                <For each=move || conversations.get() key=|item| (
                    item.username.clone(), item.un_acknowledged_message_count, item.has_un_acknowledged_messages
                ) children=move |item| {
                    let name = item.username.clone();
                    let username = item.username.clone();
                    let unread = item.un_acknowledged_message_count;
                    let has_unread = item.has_un_acknowledged_messages;
                    view! { <button class:menu-active=move || active.get() == name class="menu-item" on:click=move |_| select(username.clone())>
                        {item.username} <Show when=move || has_unread><span class="label red">{unread}</span></Show>
                    </button> }
                }/>
            </nav>
            <Show when=move || error.get().is_some()><p class="error">{move || error.get().unwrap_or_default()}</p></Show>
            <div class="chat-active-card">
                <Show when=move || !active.get().is_empty() fallback=|| view! { <p class="placeholder-segment">"Select a conversation or start a message."</p> }>
                    {move || conversations.get().into_iter().find(|c| c.username == active.get()).map(|c| view! { <conversation::ConversationView conversation=c/> })}
                </Show>
                <form class="chat-input" on:submit=move |ev| { ev.prevent_default(); send(()); }>
                    <Show when=move || active.get().is_empty()>
                        <input prop:value=move || recipient.get() on:input=move |ev| recipient.set(event_target_value(&ev)) placeholder="Username"/>
                    </Show>
                    <input id="chat-message-input" prop:value=move || draft.get() on:input=move |ev| draft.set(event_target_value(&ev)) placeholder="Message"/>
                    <button class="chat-message-button" type="submit">"Send"</button>
                </form>
                <Show when=move || !active.get().is_empty()>
                    <button type="button" on:click=move |_| remove(active.get_untracked())>"Close conversation"</button>
                </Show>
            </div>
        </section>
    }
}
