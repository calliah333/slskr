use leptos::prelude::*;
use leptos::task::spawn_local_scoped_with_cancellation;
use leptos_router::{
    NavigateOptions,
    hooks::{use_navigate, use_url},
};
use slskr_types::conversations::{Conversation, PrivateMessage};
use wasm_bindgen_futures::spawn_local;

mod conversation;

const ACTIVE_CHAT_KEY: &str = "slskr.activeChat";

fn stored_username() -> Option<String> {
    web_sys::window()?
        .session_storage()
        .ok()
        .flatten()?
        .get_item(ACTIVE_CHAT_KEY)
        .ok()
        .flatten()
}

#[component]
pub fn Chat() -> impl IntoView {
    let url = use_url();
    // UrlSearchParams percent-decodes the query exactly once.
    let initial = url
        .with_untracked(|url| {
            web_sys::UrlSearchParams::new_with_str(url.search())
                .ok()
                .and_then(|params| params.get("username"))
        })
        .or_else(stored_username)
        .unwrap_or_default();
    let navigate = StoredValue::new_local(use_navigate());
    let conversations = RwSignal::new(Vec::<Conversation>::new());
    let active = RwSignal::new(initial);
    let draft = RwSignal::new(String::new());
    let recipient = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    // Each refresh takes a ticket; only the latest one may apply its result.
    let generation = RwSignal::new(0u64);
    let in_flight = RwSignal::new(false);
    let refresh = move || {
        let selected = active.get_untracked();
        let ticket = generation.get_untracked().wrapping_add(1);
        generation.set(ticket);
        in_flight.set(true);
        let current = move || generation.try_get_untracked() == Some(ticket);
        spawn_local(async move {
            let result = load_conversations(selected).await;
            if !current() {
                return;
            }
            let _ = in_flight.try_set(false);
            match result {
                Ok((selected, value, problem)) => {
                    if let Some(problem) = problem {
                        let _ = error.try_set(Some(problem));
                    }
                    if active.try_get_untracked().is_some_and(|a| a != selected) {
                        let _ = active.try_set(selected);
                    }
                    // Unchanged polls must not notify, or the open conversation re-renders.
                    if conversations.try_with_untracked(|c| *c != value) == Some(true) {
                        let _ = conversations.try_set(value);
                    }
                }
                Err(e) => {
                    let _ = error.try_set(Some(e));
                }
            }
        });
    };
    Effect::new(move |_| refresh());
    spawn_local_scoped_with_cancellation(async move {
        loop {
            crate::services::util::sleep(5_000).await;
            if !in_flight.get_untracked() {
                refresh();
            }
        }
    });

    let select = move |username: String| {
        active.set(username.clone());
        if let Some(storage) = web_sys::window().and_then(|w| w.session_storage().ok().flatten()) {
            let _ = storage.set_item(ACTIVE_CHAT_KEY, &username);
        }
        // The router resolves the path under `web.url_base`.
        navigate.with_value(|navigate| {
            navigate(
                &format!(
                    "/chat?username={}",
                    crate::services::util::encode_uri_component(&username)
                ),
                NavigateOptions {
                    replace: true,
                    scroll: false,
                    ..Default::default()
                },
            )
        });
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
                "/conversations/{}/messages",
                crate::services::util::encode_uri_component(&username)
            );
            let body = slskr_types::conversations::SendMessage { message: text };
            match crate::services::api::post_text(&path, &body).await {
                Ok(_) => {
                    let _ = recipient.try_set(String::new());
                    // `try_set` returns `None` once the value was stored (component still alive).
                    if active.try_set(username).is_none() {
                        refresh();
                    }
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.to_string()));
                }
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
                    let Some(current) = active.try_get_untracked() else {
                        return;
                    };
                    if current == username {
                        let _ = active.try_set(String::new());
                    }
                    refresh();
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.to_string()));
                }
            }
        });
    };
    let messages = Memo::new(move |_| {
        let name = active.get();
        conversations.with(|items| {
            items
                .iter()
                .find(|c| c.username == name)
                .and_then(|c| c.messages.clone())
        })
    });
    let has_conversation = Memo::new(move |_| messages.with(Option::is_some));
    let messages = Signal::derive(move || messages.get().unwrap_or_default());

    view! {
        <section class="chat">
            <nav class="conversation-menu">
                <For each=move || conversations.get() key=|item| (
                    item.username.clone(), item.unacknowledged_count
                ) children=move |item| {
                    let name = item.username.clone();
                    let username = item.username.clone();
                    let unread = item.unacknowledged_count;
                    let has_unread = unread > 0;
                    view! { <button class:menu-active=move || active.get() == name class="menu-item" on:click=move |_| select(username.clone())>
                        {item.username} <Show when=move || has_unread><span class="label red">{unread}</span></Show>
                    </button> }
                }/>
            </nav>
            <Show when=move || error.get().is_some()><p class="error">{move || error.get().unwrap_or_default()}</p></Show>
            <div class="chat-active-card">
                <Show when=move || !active.get().is_empty() fallback=|| view! { <p class="placeholder-segment">"Select a conversation or start a message."</p> }>
                    <Show when=move || has_conversation.get()>
                        <conversation::ConversationView messages=messages/>
                    </Show>
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

/// Fetches the conversation list and the selected conversation's messages.
/// Returns the effective selection, the list, and a non-fatal error.
async fn load_conversations(
    selected: String,
) -> Result<(String, Vec<Conversation>, Option<String>), String> {
    let mut value = crate::services::api::get::<Vec<Conversation>>("/conversations")
        .await
        .map_err(|e| e.to_string())?;
    value.retain(|conversation| conversation.username != "..");
    let selected = if value.iter().any(|c| c.username == selected) {
        selected
    } else {
        value
            .first()
            .map(|c| c.username.clone())
            .unwrap_or_default()
    };
    let mut problem = None;
    if !selected.is_empty() {
        let path = format!(
            "/conversations/{}",
            crate::services::util::encode_uri_component(&selected)
        );
        let unread = value
            .iter()
            .any(|c| c.username == selected && c.unacknowledged_count > 0);
        if unread
            && let Err(e) = crate::services::api::post_empty::<()>(&format!("{path}/ack")).await
        {
            problem = Some(e.to_string());
        }
        match crate::services::api::get::<Conversation>(&path).await {
            Ok(full) => {
                if let Some(item) = value.iter_mut().find(|c| c.username == selected) {
                    *item = full;
                }
            }
            Err(e) => problem = Some(e.to_string()),
        }
    }
    Ok((selected, value, problem))
}

/// Identity of the newest message, used to scroll only when the history grows.
pub(crate) fn last_message_key(messages: &[PrivateMessage]) -> Option<(i64, i32)> {
    messages
        .last()
        .map(|m| (m.timestamp.timestamp_millis(), m.id))
}
