use leptos::prelude::*;
use slskr_api::rooms::RoomInfo;
use wasm_bindgen_futures::spawn_local;

#[component]
pub fn RoomJoin(on_join: Callback<String>) -> impl IntoView {
    let open = RwSignal::new(false);
    let rooms = RwSignal::new(Vec::<RoomInfo>::new());
    let filter = RwSignal::new(String::new());
    let selected = RwSignal::new(String::new());
    let error = RwSignal::new(None::<String>);
    Effect::new(move |_| {
        if open.get() {
            spawn_local(async move {
                match crate::services::api::get::<Vec<RoomInfo>>("/rooms/available").await {
                    Ok(value) => rooms.set(value),
                    Err(e) => error.set(Some(e.to_string())),
                }
            });
        }
    });
    let join = move |_| {
        let name = selected.get_untracked();
        if name.is_empty() {
            return;
        }
        spawn_local(async move {
            match crate::services::api::post_text("/rooms/joined", &name).await {
                Ok(_) => {
                    on_join.run(name);
                    open.set(false);
                }
                Err(e) => error.set(Some(e.to_string())),
            }
        });
    };
    view! {
        <button class="add-button" type="button" on:click=move |_| open.set(true)>"+ Join room"</button>
        <Show when=move || open.get()>
            <div class="join-room-modal" role="dialog" aria-modal="true">
                <h2>"Join a room"</h2>
                <input prop:value=move || filter.get() on:input=move |ev| filter.set(event_target_value(&ev)) placeholder="Filter rooms"/>
                <select prop:value=move || selected.get() on:change=move |ev| selected.set(event_target_value(&ev))>
                    <option value="">"Select a room"</option>
                    <For each=move || {
                        let needle = filter.get().to_lowercase();
                        rooms.get().into_iter().filter(|room| room.name.to_lowercase().contains(&needle)).collect::<Vec<_>>()
                    } key=|room| room.name.clone() children=move |room| {
                        let name = room.name.clone();
                        view! { <option value=name.clone()>{format!("{} ({})", name, room.user_count)}</option> }
                    }/>
                </select>
                <Show when=move || error.get().is_some()><p class="error">{move || error.get().unwrap_or_default()}</p></Show>
                <button type="button" on:click=join>"Join"</button>
                <button type="button" on:click=move |_| open.set(false)>"Cancel"</button>
            </div>
        </Show>
    }
}
