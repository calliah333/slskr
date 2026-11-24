//! The "Join room" dialog: the server's room list (searchable, busiest first) and a
//! free room name.

use leptos::prelude::*;
use slskr_types::rooms::RoomInfo;
use wasm_bindgen_futures::spawn_local;

use crate::ui::{Alert, Icon, Loading, Modal};

/// Rows shown at once; the server lists thousands of rooms.
const SHOWN: usize = 150;

#[component]
pub fn RoomJoin(
    open: RwSignal<bool>,
    /// The server's room list (shared with the page, which uses it for member counts).
    rooms: RwSignal<Vec<RoomInfo>>,
    #[prop(into)] joined: Signal<Vec<String>>,
    on_join: Callback<String>,
) -> impl IntoView {
    let filter = RwSignal::new(String::new());
    let custom = RwSignal::new(String::new());
    let loading = RwSignal::new(false);
    let joining = RwSignal::new(None::<String>);
    let error = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        if !open.get() {
            return;
        }
        filter.set(String::new());
        custom.set(String::new());
        error.set(None);
        joining.set(None);
        loading.set(true);
        set_timeout(
            || {
                if let Some(el) = document().get_element_by_id("room-join-filter")
                    && let Ok(el) = wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(el)
                {
                    let _ = el.focus();
                }
            },
            std::time::Duration::from_millis(50),
        );
        spawn_local(async move {
            match crate::services::api::get::<Vec<RoomInfo>>("/rooms").await {
                Ok(value) => {
                    let _ = rooms.try_set(value);
                }
                Err(e) => {
                    let _ = error.try_set(Some(e.to_string()));
                }
            }
            let _ = loading.try_set(false);
        });
    });
    let join = move |name: String| {
        let name = name.trim().to_owned();
        if name.is_empty() || joining.get_untracked().is_some() {
            return;
        }
        if joined.with_untracked(|j| j.contains(&name)) {
            open.set(false);
            on_join.run(name);
            return;
        }
        error.set(None);
        joining.set(Some(name.clone()));
        spawn_local(async move {
            let result = crate::components::chat::buffer::join_room(&name).await;
            let _ = joining.try_set(None);
            match result {
                Ok(()) => {
                    if open.try_set(false).is_none() {
                        on_join.run(name);
                    }
                }
                Err(e) => {
                    let _ = error.try_set(Some(e));
                }
            }
        });
    };
    let matches = Memo::new(move |_| {
        let needle = filter.get().trim().to_lowercase();
        let mut list: Vec<RoomInfo> = rooms.with(|rooms| {
            rooms
                .iter()
                .filter(|room| needle.is_empty() || room.name.to_lowercase().contains(&needle))
                .cloned()
                .collect()
        });
        list.sort_by(|a, b| {
            b.user_count
                .cmp(&a.user_count)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        list
    });
    let shown =
        Memo::new(move |_| matches.with(|m| m.iter().take(SHOWN).cloned().collect::<Vec<_>>()));
    let hidden = move || matches.with(Vec::len).saturating_sub(SHOWN);

    view! {
        <Modal open=open on_close=Callback::new(move |()| open.set(false)) class="room-join"
            title="Join a room" description="Pick a room on the server or enter any name to create or join it.">
            <div class="room-join__body">
                {move || error.get().map(|e| view! {
                    <Alert variant="danger" on_dismiss=Callback::new(move |()| error.set(None))><p>{e}</p></Alert>
                })}
                <div class="input-icon">
                    <Icon name="search"/>
                    <input id="room-join-filter" class="input" type="search" placeholder="Search rooms"
                        aria-label="Search rooms" autocomplete="off"
                        prop:value=move || filter.get() on:input=move |ev| filter.set(event_target_value(&ev))/>
                </div>
                <div class="room-join__list-wrap">
                    <Show when=move || !loading.get() || !rooms.with(Vec::is_empty)
                        fallback=|| view! { <Loading label="Loading rooms" show_label=true/> }>
                        <Show when=move || !shown.with(Vec::is_empty) fallback=move || view! {
                            <p class="room-join__empty">
                                {move || if rooms.with(Vec::is_empty) {
                                    "No room list available.".to_owned()
                                } else {
                                    format!("No rooms match \u{201c}{}\u{201d}.", filter.get().trim())
                                }}
                            </p>
                        }>
                            <ul class="room-join__list" role="list" aria-label="Rooms on the server">
                                <For each=move || shown.get() key=|room| (room.name.clone(), room.user_count, room.is_private)
                                    children=move |room| {
                                        let name = room.name.clone();
                                        let target = room.name.clone();
                                        let busy_name = room.name.clone();
                                        let is_joined = {
                                            let name = room.name.clone();
                                            move || joined.with(|j| j.contains(&name))
                                        };
                                        view! {
                                            <li>
                                                <button type="button" class="room-join__item"
                                                    disabled=move || joining.get().is_some()
                                                    on:click=move |_| join(target.clone())>
                                                    <span class="buffer-item__sigil" aria-hidden="true">
                                                        {if room.is_private { view! { <Icon name="lock" size="xs"/> }.into_any() } else { "#".into_any() }}
                                                    </span>
                                                    <span class="room-join__name truncate">{name}</span>
                                                    {room.is_private.then(|| view! { <span class="badge badge--outline">"Private"</span> })}
                                                    {move || is_joined().then(|| view! { <span class="badge badge--success">"Joined"</span> })}
                                                    <span class="room-join__count" title=format!("{} users", room.user_count)>
                                                        <Icon name="users" size="xs"/>{room.user_count}
                                                    </span>
                                                    {move || (joining.get().as_deref() == Some(busy_name.as_str()))
                                                        .then(|| view! { <span class="spinner" aria-hidden="true"></span> })}
                                                </button>
                                            </li>
                                        }
                                    }/>
                            </ul>
                            <Show when=move || { hidden() > 0 }>
                                <p class="room-join__more">{move || format!("{} more rooms; search to narrow the list.", hidden())}</p>
                            </Show>
                        </Show>
                    </Show>
                </div>
                <form class="field room-join__custom" on:submit=move |ev| { ev.prevent_default(); join(custom.get_untracked()); }>
                    <label class="label" for="room-join-name">"Or enter a room name"</label>
                    <div class="input-group">
                        <input id="room-join-name" class="input" autocomplete="off" placeholder="Room name"
                            prop:value=move || custom.get() on:input=move |ev| custom.set(event_target_value(&ev))/>
                        <button type="submit" class="btn btn--primary room-join__submit"
                            disabled=move || custom.with(|c| c.trim().is_empty()) || joining.get().is_some()>
                            {move || (joining.get().is_some()).then(|| view! { <span class="spinner" aria-hidden="true"></span> })}
                            "Join"
                        </button>
                    </div>
                    <p class="help">"A room that does not exist yet is created when you join it."</p>
                </form>
            </div>
        </Modal>
    }
}
