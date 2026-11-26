//! Chat rooms (`/rooms`), IRC style: the buffer list, the open room with its topic bar
//! (ticker), line log and input line, and the nick list.

use std::{
    collections::{HashMap, HashSet},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use chrono::{DateTime, Utc};
use leptos::prelude::*;
use leptos_router::{
    NavigateOptions,
    components::A,
    hooks::{use_navigate, use_url},
};
use slskr_types::{
    conversations::Conversation,
    rooms::{Room, RoomInfo, RoomMessage, RoomUser, Ticker},
    users::UserPresence,
};
use wasm_bindgen_futures::spawn_local;

use crate::{
    components::chat::{
        buffer::{
            self, BufferView, Command, Entry, Events, HELP, InputLine, OFFLINE_REASON, PollProblem,
            Tone, announce_connection, merge, nick_color, parse_command, push_event,
            set_if_changed, use_logged_in, use_own_nick,
        },
        presence_dot,
        sidebar::{BufferList, NewMessage, QueryRow, RoomRow},
    },
    config,
    services::{
        events::{Event as LiveEvent, Status as LiveStatus, Topic, use_events},
        util,
    },
    ui::{Button, EmptyState, Icon, Loading, StatusDot},
};

mod join;

pub(crate) use join::RoomJoin;

/// Without live updates, other rooms and the conversation list are checked every this
/// many polls.
const BACKGROUND_EVERY: u32 = 5;
/// With live updates, everything is reloaded this often in case an update was missed.
const FALLBACK_POLL_SECONDS: u32 = 30;
/// Recent messages kept per room (as the daemon keeps them).
const ROOM_MESSAGE_LIMIT: usize = 25;
/// More joins and parts than this between two polls are not listed (e.g. after a reconnect).
const MAX_PRESENCE_LINES: usize = 8;

fn stored_room() -> String {
    web_sys::window()
        .and_then(|window| window.session_storage().ok().flatten())
        .and_then(|storage| storage.get_item("slskr.activeRoom").ok().flatten())
        .unwrap_or_default()
}

fn is_wide() -> bool {
    web_sys::window()
        .and_then(|w| w.match_media("(min-width: 1100px)").ok().flatten())
        .is_some_and(|m| m.matches())
}

fn presence_rank(presence: UserPresence) -> u8 {
    match presence {
        UserPresence::Online => 0,
        UserPresence::Away => 1,
        UserPresence::Offline => 2,
    }
}

/// Message keys for the log: timestamp, author and text, numbered when repeated.
fn room_entries(messages: &[RoomMessage]) -> Vec<Entry> {
    let mut seen = HashMap::<String, u32>::new();
    messages
        .iter()
        .map(|m| {
            let base = format!(
                "{}:{}:{}",
                m.timestamp.timestamp_millis(),
                m.username,
                m.message
            );
            let n = seen.entry(base.clone()).or_default();
            *n += 1;
            Entry::message(
                format!("{base}#{n}"),
                m.timestamp,
                m.username.clone(),
                m.is_self,
                m.message.clone(),
            )
        })
        .collect()
}

#[component]
pub fn Rooms() -> impl IntoView {
    let joined = RwSignal::new(Vec::<String>::new());
    let url = use_url();
    let room = url.with_untracked(|url| {
        web_sys::UrlSearchParams::new_with_str(url.search())
            .ok()?
            .get("room")
            .filter(|room| !room.is_empty())
    });
    // Phones show the buffer list first unless a room was linked.
    let show_buffer = RwSignal::new(room.is_some());
    let active = RwSignal::new(room.unwrap_or_else(stored_room));
    let navigate = StoredValue::new_local(use_navigate());
    let messages = RwSignal::new(Vec::<RoomMessage>::new());
    let users = RwSignal::new(Vec::<RoomUser>::new());
    // The room `users` belongs to; joins and parts are listed only within one room.
    let users_room = RwSignal::new(None::<String>);
    let loaded = RwSignal::new(false);
    let room_loaded = RwSignal::new(false);
    let draft = RwSignal::new(String::new());
    let ticker = RwSignal::new(String::new());
    let ticker_draft = RwSignal::new(String::new());
    let editing_ticker = RwSignal::new(false);
    // The room whose ticker was last loaded; the ticker is fetched only on room change so
    // polling never overwrites what the user is typing.
    let ticker_room = RwSignal::new(None::<String>);
    // Polling errors clear themselves; action errors become lines in the buffer.
    let poll_error = RwSignal::new(None::<String>);
    let events: Events = RwSignal::new(Vec::new());
    // Unread counts of the other rooms, and when each room was last read.
    let unread = RwSignal::new(HashMap::<String, u32>::new());
    let last_seen = StoredValue::new(HashMap::<String, DateTime<Utc>>::new());
    let room_list = RwSignal::new(Vec::<RoomInfo>::new());
    let conversations = RwSignal::new(Vec::<Conversation>::new());
    let queries_loaded = RwSignal::new(false);
    let join_open = RwSignal::new(false);
    let new_open = RwSignal::new(false);
    let members_open = RwSignal::new(is_wide());
    let member_filter = RwSignal::new(String::new());
    let logged_in = use_logged_in();
    let own_nick = use_own_nick();
    let generation = RwSignal::new(0u64);
    let in_flight = RwSignal::new(false);
    let alive = Arc::new(AtomicBool::new(true));
    let poll_alive = Arc::clone(&alive);
    announce_connection(events, logged_in);

    // `background` also recounts the other rooms' unread messages and reloads the
    // conversation list.
    let refresh_with = move |background: bool| {
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
            set_if_changed(joined, rooms.clone());
            set_if_changed(loaded, true);
            let mut problem = None;
            if selected.is_empty() {
                set_if_changed(messages, Vec::new());
                set_if_changed(users, Vec::new());
            } else {
                let encoded = util::encode_uri_component(&selected);
                // Responses for a room that is no longer active must not replace its view.
                let still_active =
                    || current() && active.try_get_untracked().as_deref() == Some(&selected);
                let value =
                    crate::services::api::get::<Room>(&format!("/rooms/joined/{encoded}")).await;
                if !still_active() {
                    finish();
                    return;
                }
                match value {
                    Ok(room) => {
                        if let Some(at) = room.messages.last().map(|m| m.timestamp) {
                            let _ = last_seen
                                .try_update_value(|seen| seen.insert(selected.clone(), at));
                        }
                        if unread.try_with_untracked(|u| u.contains_key(&selected)) == Some(true) {
                            unread.try_update(|u| u.remove(&selected));
                        }
                        set_if_changed(messages, room.messages);
                        if users_room.try_get_untracked().flatten().as_deref() == Some(&selected) {
                            announce_presence(events, &selected, users, &room.users);
                        } else {
                            let _ = users_room.try_set(Some(selected.clone()));
                        }
                        set_if_changed(users, room.users);
                    }
                    Err(e) => problem = Some(e.to_string()),
                }
                set_if_changed(room_loaded, true);
                if ticker_room.try_with_untracked(|room| room.as_deref() != Some(&selected))
                    == Some(true)
                {
                    let value = crate::services::api::get::<Ticker>(&format!(
                        "/rooms/joined/{encoded}/ticker"
                    ))
                    .await
                    .map(|value| value.ticker);
                    if !still_active() {
                        finish();
                        return;
                    }
                    let _ = ticker.try_set(value.unwrap_or_default());
                    let _ = ticker_room.try_set(Some(selected.clone()));
                }
            }
            // The other rooms' unread counts and the conversation list, less often.
            if background {
                let mut counts = HashMap::new();
                for room in rooms.iter().filter(|room| **room != selected) {
                    let value = crate::services::api::get::<Vec<RoomMessage>>(&format!(
                        "/rooms/joined/{}/messages",
                        util::encode_uri_component(room)
                    ))
                    .await;
                    if !current() {
                        return;
                    }
                    let Ok(value) = value else { continue };
                    let newest = value.last().map(|m| m.timestamp);
                    let seen = last_seen
                        .try_update_value(|seen| {
                            *seen
                                .entry(room.clone())
                                .or_insert_with(|| newest.unwrap_or(DateTime::<Utc>::MIN_UTC))
                        })
                        .unwrap_or(DateTime::<Utc>::MIN_UTC);
                    let count = value
                        .iter()
                        .filter(|m| !m.is_self && m.timestamp > seen)
                        .count();
                    if count > 0 {
                        counts.insert(room.clone(), u32::try_from(count).unwrap_or(u32::MAX));
                    }
                }
                set_if_changed(unread, counts);
                let list = crate::services::api::get::<Vec<Conversation>>("/conversations").await;
                if !current() {
                    return;
                }
                if let Ok(mut list) = list {
                    list.retain(|c| c.username != "..");
                    set_if_changed(conversations, list);
                }
                set_if_changed(queries_loaded, true);
            }
            set_if_changed(poll_error, problem);
            finish();
        });
    };

    let refresh = move || refresh_with(false);
    // A refresh asked for while one runs; `Some(true)` for a background one.
    let pending = StoredValue::new(None::<bool>);
    let request_refresh = move |background: bool| {
        if in_flight.try_get_untracked() == Some(false) {
            refresh_with(background);
        } else {
            let _ = pending.try_update_value(|p| *p = Some(background || p.unwrap_or(false)));
        }
    };
    let load_queries = move || {
        spawn_local(async move {
            let list = crate::services::api::get::<Vec<Conversation>>("/conversations").await;
            if let Ok(mut list) = list {
                list.retain(|c| c.username != "..");
                set_if_changed(conversations, list);
                set_if_changed(queries_loaded, true);
            }
        });
    };

    // The `rooms` topic adds messages and members as they come; joins and parts reload.
    let live = use_events();
    live.on(Topic::Rooms, move |event| match event {
        // (Re)subscribed, possibly after missed events.
        LiveEvent::RoomList(_) => {
            if in_flight.try_get_untracked() == Some(false) {
                refresh_with(true);
            }
        }
        LiveEvent::RoomJoined(_) | LiveEvent::RoomLeft { .. } => request_refresh(false),
        LiveEvent::RoomMessage(message) => {
            let room = &message.room;
            if users_room.try_get_untracked().flatten().as_ref() == Some(room)
                && active.try_get_untracked().as_ref() == Some(room)
            {
                let added = messages
                    .try_update(|list| {
                        if list.contains(message) {
                            return false;
                        }
                        list.push(message.clone());
                        let excess = list.len().saturating_sub(ROOM_MESSAGE_LIMIT);
                        list.drain(..excess);
                        true
                    })
                    .unwrap_or(false);
                if added {
                    let _ = last_seen
                        .try_update_value(|seen| seen.insert(room.clone(), message.timestamp));
                }
            } else if !message.is_self
                && joined.try_with_untracked(|joined| joined.contains(room)) == Some(true)
                && last_seen.try_with_value(|seen| seen.contains_key(room)) == Some(true)
            {
                unread.try_update(|unread| *unread.entry(room.clone()).or_default() += 1);
            }
        }
        // Members are listed only for the open room.
        LiveEvent::RoomUserJoined { room, user }
            if users_room.try_get_untracked().flatten().as_ref() == Some(room) =>
        {
            let mut next = users.get_untracked();
            match next.iter_mut().find(|u| u.username == user.username) {
                Some(existing) => *existing = user.clone(),
                None => next.push(user.clone()),
            }
            announce_presence(events, room, users, &next);
            set_if_changed(users, next);
        }
        LiveEvent::RoomUserLeft { room, username }
            if users_room.try_get_untracked().flatten().as_ref() == Some(room) =>
        {
            let mut next = users.get_untracked();
            next.retain(|u| &u.username != username);
            announce_presence(events, room, users, &next);
            set_if_changed(users, next);
        }
        _ => {}
    });
    live.on(Topic::Conversations, move |event| match event {
        LiveEvent::ConversationList(_) | LiveEvent::ConversationUpdated(_) => load_queries(),
        _ => {}
    });
    let socket = live.status();

    Effect::new(move |_| refresh_with(true));
    spawn_local(async move {
        let mut ticks = 0_u32;
        let mut since_refresh = 0_u32;
        while poll_alive.load(Ordering::Relaxed) {
            crate::services::util::sleep(1_000).await;
            // Skip ticks while a refresh is still outstanding so polls never overlap.
            if !poll_alive.load(Ordering::Relaxed)
                || document().hidden()
                || in_flight.try_get_untracked() != Some(false)
            {
                continue;
            }
            since_refresh += 1;
            if let Some(background) = pending.try_update_value(Option::take).flatten() {
                since_refresh = 0;
                refresh_with(background);
            } else if socket.try_get_untracked() != Some(LiveStatus::Open) {
                // Without live updates, poll every second as before.
                ticks = ticks.wrapping_add(1);
                since_refresh = 0;
                refresh_with(ticks.is_multiple_of(BACKGROUND_EVERY));
            } else if since_refresh >= FALLBACK_POLL_SECONDS {
                since_refresh = 0;
                refresh_with(true);
            }
        }
    });
    on_cleanup(move || alive.store(false, Ordering::Relaxed));

    // The server's room list gives member counts and privacy of the other joined rooms.
    Effect::new(move |_| {
        if logged_in.get() {
            spawn_local(async move {
                if let Ok(value) = crate::services::api::get::<Vec<RoomInfo>>("/rooms").await {
                    set_if_changed(room_list, value);
                }
            });
        }
    });

    let go = move |path: String| {
        navigate.with_value(|navigate| navigate(&path, NavigateOptions::default()));
    };
    let select_room = Callback::new(move |name: String| {
        if active.get_untracked() != name {
            messages.set(Vec::new());
            users.set(Vec::new());
            users_room.set(None);
            ticker.set(String::new());
            draft.set(String::new());
            editing_ticker.set(false);
            member_filter.set(String::new());
            room_loaded.set(false);
        }
        show_buffer.set(true);
        active.set(name.clone());
        let encoded = util::encode_uri_component(&name);
        if let Some(storage) = web_sys::window().and_then(|w| w.session_storage().ok().flatten()) {
            let _ = storage.set_item("slskr.activeRoom", &name);
        }
        navigate.with_value(|navigate| {
            navigate(
                &format!("/rooms?room={encoded}"),
                NavigateOptions {
                    replace: true,
                    scroll: false,
                    ..Default::default()
                },
            )
        });
        refresh();
        buffer::focus_later("room-message-input");
    });
    let error_line = move |room: String, text: String| {
        push_event(events, Some(room), Tone::Error, true, text);
    };
    let say = move |room: String, text: String| {
        if room.is_empty() {
            error_line(room, "Join a room first (/join room)".to_owned());
            draft.set(text);
            return;
        }
        if !logged_in.get_untracked() {
            error_line(room, format!("Not sent: {OFFLINE_REASON}"));
            draft.set(text);
            return;
        }
        spawn_local(async move {
            let path = format!(
                "/rooms/joined/{}/messages",
                util::encode_uri_component(&room)
            );
            let body = slskr_types::conversations::SendMessage {
                message: text.clone(),
            };
            if let Err(e) = crate::services::api::post_text(&path, &body).await {
                error_line(room.clone(), format!("Not sent: {e}"));
                // Give the text back unless something new was typed meanwhile.
                if draft.try_with_untracked(String::is_empty) == Some(true)
                    && active.try_get_untracked().as_deref() == Some(&room)
                {
                    let _ = draft.try_set(text);
                }
            }
            // Refresh only while the component (and its signals) is still alive.
            if generation.try_get_untracked().is_some() {
                refresh();
            }
        });
    };
    let start_ticker_edit = move || {
        ticker_draft.set(ticker.get_untracked());
        editing_ticker.set(true);
        buffer::focus_later("room-ticker-input");
    };
    let save_ticker = move || {
        let room = active.get_untracked();
        let text = ticker_draft.get_untracked().trim().to_owned();
        if room.is_empty() {
            return;
        }
        spawn_local(async move {
            let path = format!("/rooms/joined/{}/ticker", util::encode_uri_component(&room));
            let body = Ticker {
                ticker: text.clone(),
            };
            match crate::services::api::put::<_, ()>(&path, &body).await {
                Ok(()) => {
                    let line = if text.is_empty() {
                        "You cleared your ticker".to_owned()
                    } else {
                        format!("Your ticker is now: {text}")
                    };
                    push_event(events, Some(room.clone()), Tone::Info, true, line);
                    if active.try_get_untracked().as_deref() == Some(&room) {
                        let _ = ticker.try_set(text);
                        let _ = editing_ticker.try_set(false);
                    }
                }
                Err(e) => error_line(room, format!("Could not set the ticker: {e}")),
            }
        });
    };
    let leave_room = move || {
        let name = active.get_untracked();
        if name.is_empty() {
            return;
        }
        spawn_local(async move {
            let path = format!("/rooms/joined/{}", util::encode_uri_component(&name));
            match crate::services::api::request_text(gloo_net::http::Method::DELETE, &path, None)
                .await
            {
                Ok(_) => {
                    unread.try_update(|u| u.remove(&name));
                    if active.try_get_untracked().as_deref() == Some(&name) {
                        let _ = show_buffer.try_set(false);
                        let _ = active.try_set(String::new());
                    }
                    if generation.try_get_untracked().is_some() {
                        refresh();
                    }
                }
                Err(e) => error_line(name, format!("Could not leave: {e}")),
            }
        });
    };
    let submit = Callback::new(move |text: String| {
        let here = active.get_untracked();
        match parse_command(&text) {
            Command::Say(text) => say(here, text),
            Command::Join(room) => {
                if joined.with_untracked(|j| j.contains(&room)) {
                    select_room.run(room);
                    return;
                }
                spawn_local(async move {
                    match buffer::join_room(&room).await {
                        Ok(()) if active.try_get_untracked().as_ref() == Some(&here) => {
                            select_room.run(room.clone());
                            push_event(
                                events,
                                Some(room.clone()),
                                Tone::Info,
                                true,
                                format!("You joined #{room}"),
                            );
                        }
                        Ok(()) => {}
                        Err(e) => error_line(here, format!("Could not join #{room}: {e}")),
                    }
                });
            }
            Command::Part => {
                if here.is_empty() {
                    error_line(here, "You are not in a room".to_owned());
                } else {
                    leave_room();
                }
            }
            Command::Query(nick) => go(format!(
                "/chat?username={}",
                util::encode_uri_component(&nick)
            )),
            Command::Msg(nick, text) => spawn_local(async move {
                match buffer::send_private(&nick, &text).await {
                    Ok(()) => push_event(
                        events,
                        Some(here),
                        Tone::Info,
                        true,
                        format!("-> {nick}: {text}"),
                    ),
                    Err(e) => error_line(here, format!("Not sent to {nick}: {e}")),
                }
            }),
            Command::Help => {
                for line in HELP {
                    push_event(events, Some(here.clone()), Tone::Info, true, line);
                }
            }
            Command::Invalid(problem) => error_line(here, problem),
        }
    });

    let entries = Signal::derive(move || {
        let here = active.get();
        merge(messages.with(|m| room_entries(m)), events, &here)
    });
    let members = Memo::new(move |_| {
        let needle = member_filter.get().trim().to_lowercase();
        let mut list: Vec<RoomUser> = users.with(|users| {
            users
                .iter()
                .filter(|u| needle.is_empty() || u.username.to_lowercase().contains(&needle))
                .cloned()
                .collect()
        });
        list.sort_by(|a, b| {
            presence_rank(a.status)
                .cmp(&presence_rank(b.status))
                .then_with(|| a.username.to_lowercase().cmp(&b.username.to_lowercase()))
        });
        list
    });
    let member_count = Memo::new(move |_| users.with(Vec::len));
    let online_count = Memo::new(move |_| {
        users.with(|u| {
            u.iter()
                .filter(|u| u.status == UserPresence::Online)
                .count()
        })
    });
    let info =
        move |name: &str| room_list.with(|list| list.iter().find(|r| r.name == name).cloned());
    let active_private = Memo::new(move |_| active.with(|a| info(a).is_some_and(|r| r.is_private)));
    let offline_reason =
        Signal::derive(move || (!logged_in.get()).then(|| OFFLINE_REASON.to_owned()));
    let rooms = Memo::new(move |_| {
        let current = active.get();
        let mut rows: Vec<RoomRow> = joined.with(|joined| {
            joined
                .iter()
                .map(|name| {
                    let known = info(name);
                    let users = if *name == current && room_loaded.get() {
                        Some(member_count.get())
                    } else {
                        known.as_ref().map(|r| r.user_count as usize)
                    };
                    RoomRow {
                        name: name.clone(),
                        users,
                        unread: unread.with(|u| u.get(name).copied().unwrap_or_default()),
                        private: known.is_some_and(|r| r.is_private),
                    }
                })
                .collect()
        });
        rows.sort_by_key(|r| r.name.to_lowercase());
        rows
    });
    let queries = Memo::new(move |_| {
        let mut rows: Vec<QueryRow> = conversations.with(|list| {
            list.iter()
                .map(|c| QueryRow {
                    nick: c.username.clone(),
                    unread: c.unacknowledged_count,
                })
                .collect()
        });
        rows.sort_by_key(|r| r.nick.to_lowercase());
        rows
    });
    let base = StoredValue::new(config::url_base().trim_end_matches('/').to_owned());
    let has_room = Memo::new(move |_| !active.with(String::is_empty));

    view! {
        <div class="messenger rooms" class:has-members=move || members_open.get() && has_room.get()
            data-view=move || if show_buffer.get() { "buffer" } else { "list" }>
            <BufferList title="Rooms" rooms=rooms queries=queries
                active_room=Signal::derive(move || Some(active.get()).filter(|a| !a.is_empty()))
                active_query=Signal::derive(|| None::<String>)
                rooms_loaded=loaded queries_loaded=queries_loaded
                on_room=select_room
                on_query=Callback::new(move |nick: String| go(format!("/chat?username={}", util::encode_uri_component(&nick))))
                on_join=Callback::new(move |()| join_open.set(true))
                on_new=Callback::new(move |()| new_open.set(true))/>

            <section class="messenger__main room-active-card" aria-label="Room">
                <header class="topic">
                    <Button variant="ghost" size="sm" icon="arrow-left" label="Back to buffers"
                        class="messenger__back" on_click=Callback::new(move |_| show_buffer.set(false))/>
                    <Show when=move || has_room.get() fallback=|| view! {
                        <h2 class="topic__name">"Rooms"</h2>
                    }>
                        <h2 class="topic__name">
                            <span class="topic__sigil" aria-hidden="true">
                                {move || if active_private.get() { view! { <Icon name="lock" size="xs"/> }.into_any() } else { "#".into_any() }}
                            </span>
                            <span class="truncate">{move || active.get()}</span>
                            <span class="sr-only">{move || if active_private.get() { " (private)" } else { "" }}</span>
                        </h2>
                        <Show when=move || editing_ticker.get() fallback=move || view! {
                            <span class="topic__text room-ticker">
                                <span class="truncate" title=move || ticker.get()>
                                    {move || {
                                        let t = ticker.get();
                                        if t.is_empty() { "No ticker set".to_owned() } else { t }
                                    }}
                                </span>
                                <button type="button" class="btn btn--ghost btn--xs btn--icon room-ticker__edit"
                                    aria-label="Edit your ticker" title="Edit your ticker"
                                    on:click=move |_| start_ticker_edit()>
                                    <Icon name="pencil" size="xs"/>
                                </button>
                            </span>
                        }>
                            <form class="topic__text room-ticker__form" on:submit=move |ev| { ev.prevent_default(); save_ticker(); }
                                on:keydown=move |ev: leptos::ev::KeyboardEvent| if ev.key() == "Escape" { editing_ticker.set(false); }>
                                <input id="room-ticker-input" class="input input--sm input--mono" maxlength="1024"
                                    aria-label="Your ticker for this room" placeholder="Your ticker for this room"
                                    prop:value=move || ticker_draft.get()
                                    on:input=move |ev| ticker_draft.set(event_target_value(&ev))/>
                                <Button size="xs" icon="check" label="Save ticker" button_type="submit"/>
                                <Button variant="ghost" size="xs" icon="x" label="Cancel"
                                    on_click=Callback::new(move |_| editing_ticker.set(false))/>
                            </form>
                        </Show>
                        <div class="topic__actions">
                            <button type="button" class="btn btn--ghost btn--sm room-members-toggle"
                                aria-pressed=move || members_open.get().to_string()
                                aria-controls="room-members" title="Nick list"
                                on:click=move |_| members_open.update(|o| *o = !*o)>
                                <Icon name="users"/>
                                <span class="tabular">{move || member_count.get()}</span>
                                <span class="sr-only">" members"</span>
                            </button>
                            <Button variant="ghost" size="sm" icon="log-out" label="Leave room"
                                class="room-leave-button" on_click=Callback::new(move |_| leave_room())/>
                        </div>
                    </Show>
                </header>
                <PollProblem poll_error=poll_error/>
                <div class="messenger__body">
                    <div class="messenger__column">
                        <BufferView entries=entries scope=active own_nick=own_nick
                            label=Signal::derive(move || format!("Messages in {}", active.get()))
                            empty=ViewFn::from(move || {
                                if !has_room.get() {
                                    view! {
                                        <div class="buffer__empty">
                                            <EmptyState icon="messages-square" title="Join a room to start chatting">
                                                <Button variant="primary" size="sm" icon="plus" text="Join room"
                                                    on_click=Callback::new(move |_| join_open.set(true))/>
                                            </EmptyState>
                                        </div>
                                    }.into_any()
                                } else if room_loaded.get() {
                                    view! {
                                        <div class="buffer__empty">
                                            <p class="buffer__hint">{format!("* Now talking in #{}. Messages sent while you are here show up below.", active.get_untracked())}</p>
                                        </div>
                                    }.into_any()
                                } else {
                                    view! { <div class="buffer__empty"><Loading label="Loading messages"/></div> }.into_any()
                                }
                            })/>
                        <InputLine draft=draft on_submit=submit offline_reason=offline_reason nick=own_nick
                            input_id="room-message-input"
                            placeholder=Signal::derive(move || {
                                let room = active.get();
                                if room.is_empty() { "Type /join room to start".to_owned() } else { format!("Message #{room}") }
                            })/>
                    </div>
                    <Show when=move || members_open.get() && has_room.get()>
                        <div class="nicklist-backdrop" on:click=move |_| members_open.set(false)></div>
                        <aside class="nicklist room-users" id="room-members" aria-label="Nick list">
                            <div class="nicklist__head">
                                <span class="nicklist__title">
                                    {move || format!("{} users \u{b7} {} online", member_count.get(), online_count.get())}
                                </span>
                                <Button variant="ghost" size="xs" icon="x" label="Hide nick list"
                                    on_click=Callback::new(move |_| members_open.set(false))/>
                            </div>
                            <div class="nicklist__search">
                                <input class="input input--sm input--mono" type="search" placeholder="Filter nicks"
                                    aria-label="Filter nicks"
                                    prop:value=move || member_filter.get()
                                    on:input=move |ev| member_filter.set(event_target_value(&ev))/>
                            </div>
                            <ul class="nicklist__list" role="list">
                                <For each=move || members.get() key=|user| (user.username.clone(), user.status, user.is_self)
                                    children=move |user| {
                                        let (dot, text) = presence_dot(user.status);
                                        let href = format!("{}/users?username={}",
                                            base.get_value(), util::encode_uri_component(&user.username));
                                        let detail = format!("{} \u{b7} {} files \u{b7} {}", text,
                                            user.file_count, util::format_speed(f64::from(user.average_speed)));
                                        let class = format!("nick-row__name truncate {}", nick_color(&user.username));
                                        view! {
                                            <li class="nick-row" class:nick-row--self=user.is_self>
                                                <StatusDot status=dot label=text.to_owned()/>
                                                <A href=href attr:class=class attr:title=detail>{user.username.clone()}</A>
                                            </li>
                                        }
                                    }/>
                            </ul>
                            <Show when=move || members.with(Vec::is_empty)>
                                <p class="nicklist__empty">
                                    {move || if member_count.get() == 0 { "No users loaded." } else { "No nick matches." }}
                                </p>
                            </Show>
                        </aside>
                    </Show>
                </div>
            </section>
        </div>
        <RoomJoin open=join_open rooms=room_list joined=Signal::from(joined) on_join=select_room/>
        <NewMessage open=new_open
            on_start=Callback::new(move |nick: String| go(format!("/chat?username={}", util::encode_uri_component(&nick))))/>
    }
}

/// Lists who joined or left since the previous poll of the same room.
fn announce_presence(
    events: Events,
    room: &str,
    previous: RwSignal<Vec<RoomUser>>,
    now: &[RoomUser],
) {
    let Some((joined, left)) = previous.try_with_untracked(|before| {
        let before: HashSet<&str> = before.iter().map(|u| u.username.as_str()).collect();
        let after: HashSet<&str> = now.iter().map(|u| u.username.as_str()).collect();
        let mut joined: Vec<String> = after.difference(&before).map(|s| (*s).to_owned()).collect();
        let mut left: Vec<String> = before.difference(&after).map(|s| (*s).to_owned()).collect();
        joined.sort();
        left.sort();
        (joined, left)
    }) else {
        return;
    };
    if joined.len() + left.len() > MAX_PRESENCE_LINES {
        push_event(
            events,
            Some(room.to_owned()),
            Tone::Info,
            false,
            format!("{} joined, {} left", joined.len(), left.len()),
        );
        return;
    }
    for nick in joined {
        push_event(
            events,
            Some(room.to_owned()),
            Tone::Info,
            false,
            format!("{nick} has joined #{room}"),
        );
    }
    for nick in left {
        push_event(
            events,
            Some(room.to_owned()),
            Tone::Info,
            false,
            format!("{nick} has left #{room}"),
        );
    }
}
