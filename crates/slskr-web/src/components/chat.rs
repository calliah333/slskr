//! Private messages (`/chat`), IRC style: the buffer list beside the open query.

use leptos::prelude::*;
use leptos::task::spawn_local_scoped_with_cancellation;
use leptos_router::{
    NavigateOptions,
    hooks::{use_navigate, use_url},
};
use slskr_types::{
    conversations::{Conversation, MessageDirection},
    rooms::RoomInfo,
    users::{Status, UserPresence},
};
use wasm_bindgen_futures::spawn_local;

use crate::{
    components::app::AppContext,
    config,
    services::{
        events::{Event as LiveEvent, Status as LiveStatus, Topic, use_events},
        util,
    },
    ui::{Button, EmptyState, Icon, Loading, StatusDot},
};

pub(crate) mod buffer;
pub(crate) mod sidebar;

use buffer::{
    BufferView, Command, Entry, Events, HELP, InputLine, OFFLINE_REASON, PollProblem, Tone,
    announce_connection, merge, parse_command, push_event, set_if_changed, use_logged_in,
    use_own_nick,
};
use sidebar::{BufferList, NewMessage, QueryRow, RoomRow};

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

pub(crate) fn presence_dot(presence: UserPresence) -> (&'static str, &'static str) {
    match presence {
        UserPresence::Online => ("online", "Online"),
        UserPresence::Away => ("away", "Away"),
        UserPresence::Offline => ("offline", "Offline"),
    }
}

#[component]
pub fn Chat() -> impl IntoView {
    let url = use_url();
    // UrlSearchParams percent-decodes the query exactly once.
    let from_url = url.with_untracked(|url| {
        web_sys::UrlSearchParams::new_with_str(url.search())
            .ok()
            .and_then(|params| params.get("username"))
            .filter(|name| !name.is_empty())
    });
    // Phones show the buffer list first unless a conversation was linked.
    let show_buffer = RwSignal::new(from_url.is_some());
    // A linked user without a conversation opens a new one once the list is loaded.
    let pending_link = StoredValue::new(from_url.clone());
    let initial = from_url.or_else(stored_username).unwrap_or_default();
    let navigate = StoredValue::new_local(use_navigate());
    let conversations = RwSignal::new(Vec::<Conversation>::new());
    let loaded = RwSignal::new(false);
    let active = RwSignal::new(initial);
    // A new conversation with someone not in the list yet.
    let compose_to = RwSignal::new(None::<String>);
    let draft = RwSignal::new(String::new());
    // Polling errors clear themselves; action errors become lines in the buffer.
    let poll_error = RwSignal::new(None::<String>);
    let events: Events = RwSignal::new(Vec::new());
    let new_open = RwSignal::new(false);
    let join_open = RwSignal::new(false);
    let room_list = RwSignal::new(Vec::<RoomInfo>::new());
    let status = RwSignal::new(None::<(String, UserPresence)>);
    // Each refresh takes a ticket; only the latest one may apply its result.
    let generation = RwSignal::new(0u64);
    let in_flight = RwSignal::new(false);
    // Acknowledging needs the Soulseek server; unread messages are acknowledged by the
    // first refresh after logging in.
    let logged_in = use_logged_in();
    let own_nick = use_own_nick();
    announce_connection(events, logged_in);

    let refresh = move || {
        let selected = active.get_untracked();
        let ticket = generation.get_untracked().wrapping_add(1);
        generation.set(ticket);
        in_flight.set(true);
        let current = move || generation.try_get_untracked() == Some(ticket);
        let acknowledge = logged_in.get_untracked();
        spawn_local(async move {
            let result = load_conversations(selected, acknowledge).await;
            if !current() {
                return;
            }
            let _ = in_flight.try_set(false);
            match result {
                Ok((selected, list, problem)) => {
                    set_if_changed(poll_error, problem);
                    if let Some(link) = pending_link.try_update_value(Option::take).flatten()
                        && !list.iter().any(|c| c.username == link)
                    {
                        let _ = compose_to.try_set(Some(link));
                    }
                    if active.try_get_untracked().is_some_and(|a| a != selected) {
                        let _ = active.try_set(selected);
                    }
                    // Unchanged polls must not notify, or the open conversation re-renders.
                    set_if_changed(conversations, list);
                    set_if_changed(loaded, true);
                }
                Err(e) => set_if_changed(poll_error, Some(e)),
            }
        });
    };
    Effect::new(move |_| refresh());
    // The `conversations` topic triggers reloads; a reload asked for while one runs waits.
    let pending = StoredValue::new(false);
    let request_refresh = move || {
        if in_flight.get_untracked() {
            pending.set_value(true);
        } else {
            refresh();
        }
    };
    let live = use_events();
    live.on(Topic::Conversations, move |event| match event {
        // (Re)subscribed, possibly after missed events.
        LiveEvent::ConversationList(_) => {
            if !in_flight.get_untracked() {
                refresh();
            }
        }
        LiveEvent::ConversationMessage(_) => request_refresh(),
        LiveEvent::ConversationUpdated(update) => {
            // Our own acknowledgements come back as updates the list already shows.
            let known = conversations.with_untracked(|list| {
                list.iter().any(|c| {
                    c.username == update.username
                        && c.is_active == update.is_active
                        && c.unacknowledged_count == update.unacknowledged_count
                })
            });
            if !known {
                request_refresh();
            }
        }
        _ => {}
    });
    let socket = live.status();
    spawn_local_scoped_with_cancellation(async move {
        let mut since_refresh = 0_u32;
        loop {
            crate::services::util::sleep(1_000).await;
            if document().hidden() || in_flight.get_untracked() {
                continue;
            }
            since_refresh += 1;
            // Without live updates, poll every 5 s as before; otherwise every 30 s in case
            // an update was missed.
            let interval = if socket.get_untracked() == LiveStatus::Open {
                30
            } else {
                5
            };
            if std::mem::take(&mut *pending.write_value()) || since_refresh >= interval {
                since_refresh = 0;
                refresh();
            }
        }
    });

    // The open buffer: a new conversation, the selected one, or "" (none).
    let target = Memo::new(move |_| compose_to.get().unwrap_or_else(|| active.get()));
    // The partner's presence, when the server can tell.
    Effect::new(move |_| {
        let username = target.get();
        if !logged_in.get() || username.is_empty() {
            return;
        }
        spawn_local(async move {
            let path = format!("/users/{}/status", util::encode_uri_component(&username));
            if let Ok(value) = crate::services::api::get::<Status>(&path).await
                && target.try_get_untracked().as_deref() == Some(username.as_str())
            {
                let _ = status.try_set(Some((username, value.presence)));
            }
        });
    });

    let go = move |path: String| {
        navigate.with_value(|navigate| navigate(&path, NavigateOptions::default()));
    };
    let remember = move |username: &str| {
        if let Some(storage) = web_sys::window().and_then(|w| w.session_storage().ok().flatten()) {
            let _ = storage.set_item(ACTIVE_CHAT_KEY, username);
        }
        // The router resolves the path under `web.url_base`.
        navigate.with_value(|navigate| {
            navigate(
                &format!("/chat?username={}", util::encode_uri_component(username)),
                NavigateOptions {
                    replace: true,
                    scroll: false,
                    ..Default::default()
                },
            )
        });
    };
    let select = move |username: String| {
        compose_to.set(None);
        show_buffer.set(true);
        if active.get_untracked() != username {
            draft.set(String::new());
        }
        active.set(username.clone());
        remember(&username);
        refresh();
        buffer::focus_later("chat-message-input");
    };
    let start = Callback::new(move |username: String| {
        let exists = conversations.with_untracked(|c| c.iter().any(|c| c.username == username));
        if exists {
            select(username);
        } else {
            compose_to.set(Some(username.clone()));
            draft.set(String::new());
            show_buffer.set(true);
            remember(&username);
            buffer::focus_later("chat-message-input");
        }
    });
    let error_line = move |buffer: String, text: String| {
        push_event(events, Some(buffer), Tone::Error, true, text);
    };
    let remove = move |username: String| {
        spawn_local(async move {
            let path = format!("/conversations/{}", util::encode_uri_component(&username));
            match crate::services::api::request_text(gloo_net::http::Method::DELETE, &path, None)
                .await
            {
                Ok(_) => {
                    let Some(current) = active.try_get_untracked() else {
                        return;
                    };
                    if current == username {
                        let _ = active.try_set(String::new());
                        let _ = show_buffer.try_set(false);
                    }
                    refresh();
                }
                Err(e) => error_line(username, format!("Could not close the conversation: {e}")),
            }
        });
    };
    let say = move |username: String, text: String| {
        if username.is_empty() {
            error_line(
                username,
                "Open a conversation first (/query nick)".to_owned(),
            );
            draft.set(text);
            return;
        }
        if !logged_in.get_untracked() {
            error_line(username, format!("Not sent: {OFFLINE_REASON}"));
            draft.set(text);
            return;
        }
        spawn_local(async move {
            match buffer::send_private(&username, &text).await {
                Ok(()) => {
                    if target.try_get_untracked().as_deref() != Some(&username) {
                        return;
                    }
                    if compose_to.try_get_untracked().flatten().as_deref() == Some(&username) {
                        let _ = compose_to.try_set(None);
                    }
                    // `try_set` returns `None` once the value was stored (component still alive).
                    if active.try_set(username).is_none() {
                        refresh();
                    }
                }
                Err(e) => {
                    error_line(username.clone(), format!("Not sent: {e}"));
                    // Give the text back unless something new was typed meanwhile.
                    if target.try_get_untracked().as_deref() == Some(&username)
                        && draft.try_with_untracked(String::is_empty) == Some(true)
                    {
                        let _ = draft.try_set(text);
                    }
                }
            }
        });
    };
    let submit = Callback::new(move |text: String| {
        let here = target.get_untracked();
        match parse_command(&text) {
            Command::Say(text) => say(here, text),
            Command::Join(room) => spawn_local(async move {
                match buffer::join_room(&room).await {
                    Ok(()) if target.try_get_untracked().as_ref() == Some(&here) => {
                        go(format!("/rooms?room={}", util::encode_uri_component(&room)));
                    }
                    Ok(()) => {}
                    Err(e) => error_line(here, format!("Could not join #{room}: {e}")),
                }
            }),
            Command::Part => {
                if compose_to.get_untracked().is_some() {
                    compose_to.set(None);
                } else if !here.is_empty() {
                    remove(here);
                }
            }
            Command::Query(nick) => start.run(nick),
            Command::Msg(nick, text) => spawn_local(async move {
                match buffer::send_private(&nick, &text).await {
                    Ok(()) if target.try_get_untracked().as_ref() == Some(&here) => select(nick),
                    Ok(()) => {}
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

    let messages = Memo::new(move |_| {
        let name = active.get();
        conversations.with(|items| {
            items
                .iter()
                .find(|c| c.username == name)
                .and_then(|c| c.messages.clone())
        })
    });
    let entries = Signal::derive(move || {
        let here = target.get();
        let lines = if compose_to.with(Option::is_some) {
            Vec::new()
        } else {
            let own = own_nick.get();
            messages.with(|m| {
                m.as_deref()
                    .unwrap_or_default()
                    .iter()
                    .map(|m| {
                        let mine = m.direction == MessageDirection::Out;
                        Entry::message(
                            format!("{}:{}", m.timestamp.timestamp_millis(), m.id),
                            m.timestamp,
                            if mine {
                                own.clone()
                            } else {
                                m.username.clone()
                            },
                            mine,
                            m.message.clone(),
                        )
                    })
                    .collect()
            })
        };
        merge(lines, events, &here)
    });
    let buffer_loaded =
        Memo::new(move |_| compose_to.with(Option::is_some) || messages.with(Option::is_some));

    let queries = Memo::new(move |_| {
        let mut rows: Vec<QueryRow> = conversations.with(|list| {
            list.iter()
                .map(|c| QueryRow {
                    nick: c.username.clone(),
                    unread: c.unacknowledged_count,
                })
                .collect()
        });
        if let Some(nick) = compose_to.get()
            && !rows.iter().any(|r| r.nick == nick)
        {
            rows.push(QueryRow { nick, unread: 0 });
        }
        rows.sort_by_key(|r| r.nick.to_lowercase());
        rows
    });
    let joined_rooms = use_context::<AppContext>().map(|c| c.state);
    let rooms = Signal::derive(move || {
        let mut rows: Vec<RoomRow> = joined_rooms
            .map(|state| {
                state.with(|s| {
                    s.rooms
                        .iter()
                        .map(|name| RoomRow {
                            name: name.clone(),
                            users: None,
                            unread: 0,
                            private: false,
                        })
                        .collect()
                })
            })
            .unwrap_or_default();
        rows.sort_by_key(|r| r.name.to_lowercase());
        rows
    });
    let joined_names = Signal::derive(move || {
        rooms.with(|r| r.iter().map(|r| r.name.clone()).collect::<Vec<_>>())
    });

    let presence = Signal::derive(move || {
        let name = target.get();
        status.with(|s| match s {
            Some((user, presence)) if *user == name => Some(*presence),
            _ => None,
        })
    });
    let offline_reason =
        Signal::derive(move || (!logged_in.get()).then(|| OFFLINE_REASON.to_owned()));
    let base = StoredValue::new(config::url_base().trim_end_matches('/').to_owned());
    let user_link = move |page: &str| {
        format!(
            "{}/{page}?username={}",
            base.get_value(),
            util::encode_uri_component(&target.get())
        )
    };
    let browse_link = move || user_link("browse");
    let info_link = move || user_link("users");
    let has_target = Memo::new(move |_| !target.with(String::is_empty));

    view! {
        <div class="messenger chat" data-view=move || if show_buffer.get() { "buffer" } else { "list" }>
            <BufferList title="Chat" rooms=rooms queries=queries
                active_room=Signal::derive(|| None::<String>)
                active_query=Signal::derive(move || Some(target.get()).filter(|t| !t.is_empty()))
                rooms_loaded=Signal::derive(|| true) queries_loaded=loaded
                on_room=Callback::new(move |room: String| go(format!("/rooms?room={}", util::encode_uri_component(&room))))
                on_query=Callback::new(move |nick: String| select(nick))
                on_join=Callback::new(move |()| join_open.set(true))
                on_new=Callback::new(move |()| new_open.set(true))/>

            <section class="messenger__main chat-active-card" aria-label="Conversation">
                <header class="topic">
                    <Button variant="ghost" size="sm" icon="arrow-left" label="Back to buffers"
                        class="messenger__back" on_click=Callback::new(move |_| show_buffer.set(false))/>
                    <Show when=move || has_target.get() fallback=|| view! {
                        <h2 class="topic__name">"Chat"</h2>
                    }>
                        <h2 class="topic__name">
                            <span class=move || format!("truncate {}", buffer::nick_color(&target.get()))>{move || target.get()}</span>
                            {move || presence.get().map(|p| {
                                let (dot, text) = presence_dot(p);
                                view! { <StatusDot status=dot label=text.to_owned()/> }
                            })}
                        </h2>
                        <span class="topic__text">
                            <span class="truncate">
                                {move || if compose_to.with(Option::is_some) {
                                    "New conversation".to_owned()
                                } else {
                                    presence.get().map(|p| presence_dot(p).1.to_owned()).unwrap_or_default()
                                }}
                            </span>
                        </span>
                        <div class="topic__actions">
                            <a class="btn btn--ghost btn--icon btn--sm" href=browse_link
                                aria-label="Browse files" title="Browse files"><Icon name="folder-open"/></a>
                            <a class="btn btn--ghost btn--icon btn--sm hide-sm" href=info_link
                                aria-label="User info" title="User info"><Icon name="user"/></a>
                            <Show when=move || compose_to.with(Option::is_none) fallback=move || view! {
                                <Button variant="ghost" size="sm" icon="x" label="Discard new conversation"
                                    on_click=Callback::new(move |_| compose_to.set(None))/>
                            }>
                                <Button variant="ghost" size="sm" icon="x" label="Close conversation"
                                    class="chat-close-button"
                                    on_click=Callback::new(move |_| remove(active.get_untracked()))/>
                            </Show>
                        </div>
                    </Show>
                </header>
                <PollProblem poll_error=poll_error/>
                <BufferView entries=entries scope=target own_nick=own_nick
                    label=Signal::derive(move || format!("Messages with {}", target.get()))
                    empty=ViewFn::from(move || {
                        if !has_target.get() {
                            view! {
                                <div class="buffer__empty">
                                    <EmptyState icon="message-square" title="No conversation open">
                                        <Button variant="primary" size="sm" icon="plus" text="New message"
                                            on_click=Callback::new(move |_| new_open.set(true))/>
                                    </EmptyState>
                                </div>
                            }.into_any()
                        } else if buffer_loaded.get() {
                            view! {
                                <div class="buffer__empty">
                                    <p class="buffer__hint">{format!("* This is the start of your conversation with {}.", target.get_untracked())}</p>
                                </div>
                            }.into_any()
                        } else {
                            view! { <div class="buffer__empty"><Loading label="Loading messages"/></div> }.into_any()
                        }
                    })/>
                <InputLine draft=draft on_submit=submit offline_reason=offline_reason nick=own_nick
                    input_id="chat-message-input"
                    placeholder=Signal::derive(move || {
                        let t = target.get();
                        if t.is_empty() { "Type /query nick to start".to_owned() } else { format!("Message {t}") }
                    })/>
            </section>
        </div>
        <NewMessage open=new_open on_start=start/>
        <crate::components::rooms::RoomJoin open=join_open rooms=room_list joined=joined_names
            on_join=Callback::new(move |room: String| go(format!("/rooms?room={}", util::encode_uri_component(&room))))/>
    }
}

/// Fetches the conversation list and the selected conversation's messages.
/// Returns the effective selection, the list, and a non-fatal error.
async fn load_conversations(
    selected: String,
    acknowledge: bool,
) -> Result<(String, Vec<Conversation>, Option<String>), String> {
    let mut list = crate::services::api::get::<Vec<Conversation>>("/conversations")
        .await
        .map_err(|e| e.to_string())?;
    list.retain(|conversation| conversation.username != "..");
    let selected = if list.iter().any(|c| c.username == selected) {
        selected
    } else {
        list.first().map(|c| c.username.clone()).unwrap_or_default()
    };
    let mut problem = None;
    if !selected.is_empty() {
        let path = format!("/conversations/{}", util::encode_uri_component(&selected));
        let unread = list
            .iter()
            .any(|c| c.username == selected && c.unacknowledged_count > 0);
        if unread
            && acknowledge
            && let Err(e) = crate::services::api::post_empty::<()>(&format!("{path}/ack")).await
        {
            problem = Some(e.to_string());
        }
        match crate::services::api::get::<Conversation>(&path).await {
            Ok(full) => {
                if let Some(item) = list.iter_mut().find(|c| c.username == selected) {
                    *item = full;
                }
            }
            Err(e) => problem = Some(e.to_string()),
        }
    }
    Ok((selected, list, problem))
}
