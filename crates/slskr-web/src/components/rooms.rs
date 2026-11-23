use crate::{config, services::util};
use leptos::{html, prelude::*};
use leptos_router::{
    NavigateOptions,
    components::A,
    hooks::{use_navigate, use_url},
};
use slskr_api::rooms::{RoomMessage, UserData};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use wasm_bindgen_futures::spawn_local;

mod join;

fn stored_room() -> String {
    web_sys::window()
        .and_then(|window| window.session_storage().ok().flatten())
        .and_then(|storage| storage.get_item("slskr.activeRoom").ok().flatten())
        .unwrap_or_default()
}

fn set_if_changed<T: PartialEq + Send + Sync + 'static>(signal: RwSignal<T>, value: T) {
    if signal.try_with_untracked(|current| *current != value) == Some(true) {
        let _ = signal.try_set(value);
    }
}

#[component]
pub fn Rooms() -> impl IntoView {
    let joined = RwSignal::new(Vec::<String>::new());
    let url = use_url();
    let room = url.with_untracked(|url| {
        web_sys::UrlSearchParams::new_with_str(url.search())
            .ok()?
            .get("room")
    });
    let active = RwSignal::new(room.unwrap_or_else(stored_room));
    let navigate = use_navigate();
    let messages = RwSignal::new(Vec::<RoomMessage>::new());
    let users = RwSignal::new(Vec::<UserData>::new());
    let draft = RwSignal::new(String::new());
    let ticker = RwSignal::new(String::new());
    // The room whose ticker was last loaded; the ticker is fetched only on room change so
    // polling never overwrites what the user is typing.
    let ticker_room = RwSignal::new(None::<String>);
    // Errors from user actions persist until the next action; polling errors are
    // tracked separately and cleared once polling succeeds again.
    let error = RwSignal::new(None::<String>);
    let poll_error = RwSignal::new(None::<String>);
    let generation = RwSignal::new(0u64);
    let in_flight = RwSignal::new(false);
    let alive = Arc::new(AtomicBool::new(true));
    let poll_alive = Arc::clone(&alive);

    let refresh = move || {
        let ticket = generation.get_untracked().wrapping_add(1);
        generation.set(ticket);
        in_flight.set(true);
        // A newer refresh supersedes this one; its results are dropped.
        let current = move || generation.try_get_untracked() == Some(ticket);
        // Only the latest refresh owns the in-flight flag.
        let finish = move || {
            if current() {
                let _ = in_flight.try_set(false);
            }
        };
        spawn_local(async move {
            let response = crate::services::api::get::<Vec<String>>("/rooms/joined").await;
            let Some(active_name) = active.try_get_untracked().filter(|_| current()) else {
                return;
            };
            let rooms = match response {
                Ok(rooms) => rooms,
                Err(e) => {
                    set_if_changed(poll_error, Some(e.to_string()));
                    finish();
                    return;
                }
            };
            let selected = if rooms.contains(&active_name) {
                active_name
            } else {
                let first = rooms.first().cloned().unwrap_or_default();
                if first != active_name {
                    let _ = active.try_set(first.clone());
                }
                first
            };
            set_if_changed(joined, rooms);
            let mut problem = None;
            if selected.is_empty() {
                set_if_changed(messages, Vec::new());
                set_if_changed(users, Vec::new());
            } else {
                let encoded = util::encode_uri_component(&selected);
                // Responses for a room that is no longer active must not replace its view.
                let still_active =
                    || current() && active.try_get_untracked().as_deref() == Some(&selected);
                let value = crate::services::api::get::<Vec<RoomMessage>>(&format!(
                    "/rooms/joined/{encoded}/messages"
                ))
                .await;
                if !still_active() {
                    finish();
                    return;
                }
                match value {
                    Ok(value) => set_if_changed(messages, value),
                    Err(e) => problem = Some(e.to_string()),
                }
                let value = crate::services::api::get::<Vec<UserData>>(&format!(
                    "/rooms/joined/{encoded}/users"
                ))
                .await;
                if !still_active() {
                    finish();
                    return;
                }
                match value {
                    Ok(value) => set_if_changed(users, value),
                    Err(e) => problem = Some(e.to_string()),
                }
                if ticker_room.try_with_untracked(|room| room.as_deref() != Some(&selected))
                    == Some(true)
                {
                    let value = crate::services::api::get::<String>(&format!(
                        "/rooms/joined/{encoded}/ticker"
                    ))
                    .await;
                    if !still_active() {
                        finish();
                        return;
                    }
                    let _ = ticker.try_set(value.unwrap_or_default());
                    let _ = ticker_room.try_set(Some(selected));
                }
            }
            set_if_changed(poll_error, problem);
            finish();
        });
    };

    Effect::new(move |_| refresh());
    spawn_local(async move {
        while poll_alive.load(Ordering::Relaxed) {
            crate::services::util::sleep(1_000).await;
            // Skip ticks while a refresh is still outstanding so polls never overlap.
            if poll_alive.load(Ordering::Relaxed) && in_flight.try_get_untracked() == Some(false) {
                refresh();
            }
        }
    });
    on_cleanup(move || alive.store(false, Ordering::Relaxed));

    let select_room = Callback::new(move |name: String| {
        if active.get_untracked() != name {
            messages.set(Vec::new());
            users.set(Vec::new());
            ticker.set(String::new());
        }
        active.set(name.clone());
        error.set(None);
        let encoded = util::encode_uri_component(&name);
        if let Some(storage) = web_sys::window().and_then(|w| w.session_storage().ok().flatten()) {
            let _ = storage.set_item("slskr.activeRoom", &name);
        }
        navigate(
            &format!("/rooms?room={encoded}"),
            NavigateOptions {
                replace: true,
                scroll: false,
                ..Default::default()
            },
        );
        refresh();
    });
    let send = move |_| {
        let room = active.get_untracked();
        let text = draft.get_untracked();
        if room.is_empty() || text.trim().is_empty() {
            return;
        }
        draft.set(String::new());
        error.set(None);
        spawn_local(async move {
            let path = format!(
                "/rooms/joined/{}/messages",
                crate::services::util::encode_uri_component(&room)
            );
            if let Err(e) = crate::services::api::post_text(&path, &text).await {
                let _ = error.try_set(Some(e.to_string()));
            }
            // Refresh only while the component (and its signals) is still alive.
            if generation.try_get_untracked().is_some() {
                refresh();
            }
        });
    };
    let set_ticker = move |_| {
        let room = active.get_untracked();
        let text = ticker.get_untracked();
        if room.is_empty() {
            return;
        }
        error.set(None);
        spawn_local(async move {
            let path = format!(
                "/rooms/joined/{}/ticker",
                crate::services::util::encode_uri_component(&room)
            );
            if let Err(e) = crate::services::api::post_text(&path, &text).await {
                let _ = error.try_set(Some(e.to_string()));
            }
        });
    };

    let leave_room = move |_| {
        let name = active.get_untracked();
        if name.is_empty() {
            return;
        }
        error.set(None);
        spawn_local(async move {
            let path = format!(
                "/rooms/joined/{}",
                crate::services::util::encode_uri_component(&name)
            );
            match crate::services::api::request_text(gloo_net::http::Method::DELETE, &path, None)
                .await
            {
                Ok(_) => {
                    if active.try_set(String::new()).is_none() {
                        refresh();
                    }
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.to_string()));
                }
            }
        });
    };
    let anchor = NodeRef::<html::Div>::new();
    // Scroll to the newest message when the history grows or the room changes
    // (upstream Rooms.jsx scrolls the list's last child into view).
    let scrolled_to = StoredValue::new(None::<Option<RoomMessage>>);
    Effect::new(move |_| {
        let last = messages.with(|m| m.last().cloned());
        if let Some(anchor) = anchor.get()
            && scrolled_to.with_value(|s| s.as_ref() != Some(&last))
        {
            scrolled_to.set_value(Some(last));
            anchor.scroll_into_view();
        }
    });
    let shown_error = move || error.get().or_else(|| poll_error.get());
    view! {
        <section class="rooms">
            <nav class="room-menu">
                <For each=move || joined.get() key=|name| name.clone() children=move |name| {
                    let item_name = name.clone();
                    let room_name = name.clone();
                    view! { <button class:menu-active=move || active.get() == item_name class="menu-item" on:click=move |_| select_room.run(room_name.clone())>{name}</button> }
                }/>
                <join::RoomJoin on_join=select_room/>
            </nav>
            <Show when=move || shown_error().is_some()>
                <p class="error">{move || shown_error().unwrap_or_default()}</p>
            </Show>
            <Show when=move || !active.get().is_empty() fallback=|| view! { <p class="placeholder-segment">"Join a room to start chatting."</p> }>
                <div class="room-active-card">
                    <button type="button" on:click=leave_room>"Leave room"</button>
                    <div class="room-history">
                        <For each=move || messages.get() key=|message| format!("{}:{}:{}", message.timestamp, message.username, message.message) children=move |message| {
                            let own = message.self_.unwrap_or(false);
                            view! { <div class:room-message-self=own class="room-message">
                                <time class="room-message-time">{message.timestamp.format("%Y-%m-%d %H:%M:%S").to_string()}</time>
                                <span class="room-message-name">{message.username} ": "</span><span>{message.message}</span>
                            </div> }
                        }/>
                        <div id="room-history-scroll-anchor" node_ref=anchor></div>
                    </div>
                    <form class="room-input" on:submit=move |ev| { ev.prevent_default(); send(()); }>
                        <input id="room-message-input" prop:value=move || draft.get() on:input=move |ev| draft.set(event_target_value(&ev)) placeholder="Message room"/>
                        <button class="room-message-button" type="submit">"Send"</button>
                    </form>
                    <form on:submit=move |ev| { ev.prevent_default(); set_ticker(()); }>
                        <input prop:value=move || ticker.get() on:input=move |ev| ticker.set(event_target_value(&ev)) placeholder="Room ticker"/>
                        <button type="submit">"Set ticker"</button>
                    </form>
                </div>
                <aside class="room-users"><h3>"Users"</h3>
                    <For each=move || users.get() key=|user| user.username.clone() children=move |user| {
                        let username = user.username;
                        let href = format!("{}/users?username={}", config::url_base().trim_end_matches('/'), util::encode_uri_component(&username));
                        view! { <A href=href>{username}</A> }
                    }/>
                </aside>
            </Show>
        </section>
    }
}
