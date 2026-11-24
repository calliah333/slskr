//! The buffer list shared by Chat and Rooms (sections "Rooms" and "Messages", like an
//! IRC client) and the "New message" dialog.

use leptos::prelude::*;

use super::buffer::focus_later;
use crate::ui::{Icon, Modal};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct RoomRow {
    pub name: String,
    pub users: Option<usize>,
    pub unread: u32,
    pub private: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct QueryRow {
    pub nick: String,
    pub unread: u32,
}

#[component]
pub(crate) fn BufferList(
    /// The page title (the sidebar heading).
    title: &'static str,
    #[prop(into)] rooms: Signal<Vec<RoomRow>>,
    #[prop(into)] queries: Signal<Vec<QueryRow>>,
    /// The open room, if the open buffer is a room.
    #[prop(into)]
    active_room: Signal<Option<String>>,
    /// The open conversation, if the open buffer is a query.
    #[prop(into)]
    active_query: Signal<Option<String>>,
    #[prop(into)] rooms_loaded: Signal<bool>,
    #[prop(into)] queries_loaded: Signal<bool>,
    on_room: Callback<String>,
    on_query: Callback<String>,
    on_join: Callback<()>,
    on_new: Callback<()>,
) -> impl IntoView {
    view! {
        <aside class="messenger__sidebar buffers" aria-label="Buffers">
            <h1 class="sr-only">{title}</h1>
            <section class="buffers__section" aria-labelledby="buffers-rooms">
                <div class="buffers__head">
                    <h2 class="buffers__title" id="buffers-rooms">"Rooms"</h2>
                    <button type="button" class="btn btn--ghost btn--xs btn--icon room-join-button"
                        aria-label="Join room" title="Join room" on:click=move |_| on_join.run(())>
                        <Icon name="plus"/>
                    </button>
                </div>
                <Show when=move || rooms_loaded.get() fallback=|| view! { <p class="buffers__empty">"Loading\u{2026}"</p> }>
                    <Show when=move || !rooms.with(Vec::is_empty) fallback=move || view! {
                        <button type="button" class="buffers__empty buffers__empty--action" on:click=move |_| on_join.run(())>
                            "No rooms joined. Join one\u{2026}"
                        </button>
                    }>
                        <ul class="buffers__list room-menu" role="list">
                            <For each=move || rooms.get() key=|row| row.clone() children=move |row| {
                                let name = row.name.clone();
                                let is_active = move || active_room.with(|a| a.as_deref() == Some(name.as_str()));
                                let target = row.name.clone();
                                view! {
                                    <li>
                                        <button type="button" class="buffer-item room-item"
                                            class:is-active=is_active.clone() class:is-unread={row.unread > 0}
                                            aria-current=move || is_active().then_some("page")
                                            on:click=move |_| on_room.run(target.clone())>
                                            <span class="buffer-item__sigil" aria-hidden="true">
                                                {if row.private { view! { <Icon name="lock" size="xs"/> }.into_any() } else { "#".into_any() }}
                                            </span>
                                            <span class="buffer-item__name truncate">{row.name.clone()}</span>
                                            {(row.unread > 0).then(|| view! {
                                                <span class="count">{row.unread}<span class="sr-only">" unread"</span></span>
                                            })}
                                            {(row.unread == 0).then(|| row.users.map(|n| view! {
                                                <span class="buffer-item__meta" title=format!("{n} users")>{n}</span>
                                            })).flatten()}
                                        </button>
                                    </li>
                                }
                            }/>
                        </ul>
                    </Show>
                </Show>
            </section>
            <section class="buffers__section" aria-labelledby="buffers-queries">
                <div class="buffers__head">
                    <h2 class="buffers__title" id="buffers-queries">"Messages"</h2>
                    <button type="button" class="btn btn--ghost btn--xs btn--icon chat-new-button"
                        aria-label="New message" title="New message" on:click=move |_| on_new.run(())>
                        <Icon name="plus"/>
                    </button>
                </div>
                <Show when=move || queries_loaded.get() fallback=|| view! { <p class="buffers__empty">"Loading\u{2026}"</p> }>
                    <Show when=move || !queries.with(Vec::is_empty) fallback=move || view! {
                        <button type="button" class="buffers__empty buffers__empty--action" on:click=move |_| on_new.run(())>
                            "No conversations. Message someone\u{2026}"
                        </button>
                    }>
                        <ul class="buffers__list conversation-menu" role="list">
                            <For each=move || queries.get() key=|row| row.clone() children=move |row| {
                                let nick = row.nick.clone();
                                let is_active = move || active_query.with(|a| a.as_deref() == Some(nick.as_str()));
                                let target = row.nick.clone();
                                view! {
                                    <li>
                                        <button type="button" class="buffer-item conversation-item"
                                            class:is-active=is_active.clone() class:is-unread={row.unread > 0}
                                            aria-current=move || is_active().then_some("page")
                                            on:click=move |_| on_query.run(target.clone())>
                                            <span class="buffer-item__sigil" aria-hidden="true">"@"</span>
                                            <span class=format!("buffer-item__name truncate {}", super::buffer::nick_color(&row.nick))>
                                                {row.nick.clone()}
                                            </span>
                                            {(row.unread > 0).then(|| view! {
                                                <span class="count">{row.unread}<span class="sr-only">" unread"</span></span>
                                            })}
                                        </button>
                                    </li>
                                }
                            }/>
                        </ul>
                    </Show>
                </Show>
            </section>
        </aside>
    }
}

/// The "New message" dialog: asks for a nickname.
#[component]
pub(crate) fn NewMessage(open: RwSignal<bool>, on_start: Callback<String>) -> impl IntoView {
    let username = RwSignal::new(String::new());
    let problem = RwSignal::new(None::<&'static str>);
    Effect::new(move |_| {
        if open.get() {
            username.set(String::new());
            problem.set(None);
            focus_later("chat-new-username");
        }
    });
    let submit = move || {
        let name = username.get_untracked().trim().to_owned();
        if name.is_empty() {
            problem.set(Some("Enter a username."));
        } else if name == ".." {
            problem.set(Some("That is not a valid username."));
        } else {
            open.set(false);
            on_start.run(name);
        }
    };
    view! {
        <Modal open=open on_close=Callback::new(move |()| open.set(false)) size="sm"
            title="New message" description="Open a private conversation with a Soulseek user."
            footer=ViewFn::from(move || view! {
                <button type="button" class="btn" on:click=move |_| open.set(false)>"Cancel"</button>
                <button type="button" class="btn btn--primary chat-new-start" on:click=move |_| submit()>"Open conversation"</button>
            })>
            <form class="field" on:submit=move |ev| { ev.prevent_default(); submit(); }>
                <label class="label" for="chat-new-username">"Username"</label>
                <input class="input input--mono" id="chat-new-username" autocomplete="off" spellcheck="false"
                    aria-invalid=move || problem.with(Option::is_some).then_some("true")
                    aria-describedby="chat-new-username-error"
                    prop:value=move || username.get()
                    on:input=move |ev| { username.set(event_target_value(&ev)); problem.set(None); }/>
                {move || problem.get().map(|p| view! { <p class="error" id="chat-new-username-error" role="alert">{p}</p> })}
                <p class="help">"Tip: type "<code>"/query nick"</code>" in any input line."</p>
            </form>
        </Modal>
    }
}
