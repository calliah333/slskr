use crate::{config, services::util};
use leptos_router::{
    NavigateOptions,
    components::A,
    hooks::{use_navigate, use_url},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

fn stored_room() -> String {
    web_sys::window()
        .and_then(|window| window.session_storage().ok().flatten())
        .and_then(|storage| storage.get_item("slskr.activeRoom").ok().flatten())
        .unwrap_or_default()
}

use leptos::prelude::*;
use slskr_api::rooms::{RoomMessage, UserData};
use wasm_bindgen_futures::spawn_local;

mod join;

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
    let error = RwSignal::new(None::<String>);
    let alive = Arc::new(AtomicBool::new(true));
    let poll_alive = Arc::clone(&alive);

    let refresh = move || {
        spawn_local(async move {
            let response = crate::services::api::get::<Vec<String>>("/rooms/joined").await;
            let Some(active_name) = active.try_get_untracked() else {
                return;
            };
            match response {
                Ok(rooms) => {
                    let selected = if rooms.contains(&active_name) {
                        active_name
                    } else {
                        let first = rooms.first().cloned().unwrap_or_default();
                        if first != active_name {
                            active.set(first.clone());
                        }
                        first
                    };
                    joined.set(rooms);
                    if !selected.is_empty() {
                        let encoded = crate::services::util::encode_uri_component(&selected);
                        let messages_path = format!("/rooms/joined/{encoded}/messages");
                        let users_path = format!("/rooms/joined/{encoded}/users");
                        match crate::services::api::get::<Vec<RoomMessage>>(&messages_path).await {
                            Ok(value) => messages.set(value),
                            Err(e) => error.set(Some(e.to_string())),
                        }
                        match crate::services::api::get::<Vec<UserData>>(&users_path).await {
                            Ok(value) => users.set(value),
                            Err(e) => error.set(Some(e.to_string())),
                        }
                        if let Ok(value) = crate::services::api::get::<String>(&format!(
                            "/rooms/joined/{encoded}/ticker"
                        ))
                        .await
                        {
                            ticker.set(value);
                        }
                    } else {
                        messages.set(Vec::new());
                        users.set(Vec::new());
                    }
                    error.set(None);
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };

    Effect::new(move |_| refresh());
    spawn_local(async move {
        while poll_alive.load(Ordering::Relaxed) {
            crate::services::util::sleep(1_000).await;
            if poll_alive.load(Ordering::Relaxed) {
                refresh();
            }
        }
    });
    on_cleanup(move || alive.store(false, Ordering::Relaxed));

    let select_room = Callback::new(move |name: String| {
        active.set(name.clone());
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
        spawn_local(async move {
            let path = format!(
                "/rooms/joined/{}/messages",
                crate::services::util::encode_uri_component(&room)
            );
            if let Err(e) = crate::services::api::post_text(&path, &text).await {
                error.set(Some(e.to_string()));
            }
            refresh();
        });
    };
    let set_ticker = move |_| {
        let room = active.get_untracked();
        let text = ticker.get_untracked();
        if room.is_empty() {
            return;
        }
        spawn_local(async move {
            let path = format!(
                "/rooms/joined/{}/ticker",
                crate::services::util::encode_uri_component(&room)
            );
            if let Err(e) = crate::services::api::post_text(&path, &text).await {
                error.set(Some(e.to_string()));
            }
        });
    };

    let leave_room = move |_| {
        let name = active.get_untracked();
        if name.is_empty() {
            return;
        }
        spawn_local(async move {
            let path = format!(
                "/rooms/joined/{}",
                crate::services::util::encode_uri_component(&name)
            );
            match crate::services::api::request_text(gloo_net::http::Method::DELETE, &path, None)
                .await
            {
                Ok(_) => {
                    active.set(String::new());
                    refresh();
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };
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
            <Show when=move || error.get().is_some()>
                <p class="error">{move || error.get().unwrap_or_default()}</p>
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
                        <div id="room-history-scroll-anchor"></div>
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
