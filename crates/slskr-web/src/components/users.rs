use crate::{
    config,
    services::{api, util},
};
use leptos::prelude::*;
use leptos_router::{
    components::A,
    hooks::{use_navigate, use_url},
};
use slskr_api::users::{Endpoint, Info, Status, UserPresence};

#[component]
pub fn Users() -> impl IntoView {
    let url = use_url();
    // Read the raw query so a literal percent escape in a username is decoded only once.
    let username = Memo::new(move |_| {
        url.with(|url| {
            web_sys::UrlSearchParams::new_with_str(url.search())
                .ok()
                .and_then(|params| params.get("username"))
                .unwrap_or_default()
        })
    });
    let input = RwSignal::new(username.get_untracked());
    let info = RwSignal::new(None::<Info>);
    let status = RwSignal::new(None::<Status>);
    let endpoint = RwSignal::new(None::<Endpoint>);
    let loading = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let generation = RwSignal::new(0u64);
    let load = move |name: String| {
        let searching = !name.is_empty();
        input.set(name.clone());
        info.set(None);
        status.set(None);
        endpoint.set(None);
        error.set(None);
        loading.set(searching);
        let ticket = generation.get_untracked().wrapping_add(1);
        generation.set(ticket);
        if searching {
            fetch_user(
                name, ticket, info, status, endpoint, loading, error, generation,
            );
        }
    };
    Effect::new(move |_| load(username.get()));

    let navigate = use_navigate();
    let search = Callback::new({
        let navigate = navigate.clone();
        move |()| {
            let name = input.get_untracked().trim().to_owned();
            if name.is_empty() {
                return;
            }
            if name == username.get_untracked() {
                load(name);
            } else {
                let encoded = util::encode_uri_component(&name);
                navigate(&format!("/users?username={encoded}"), Default::default());
            }
        }
    });
    let clear = Callback::new(move |()| navigate("/users", Default::default()));
    view! {
        <div class="users-container">
            <section class="ui raised segment users-segment">
                <span class="users-segment-icon" aria-hidden="true">"Users"</span>
                <input class="users-input" type="search" placeholder="Username" prop:value=move || input.get()
                    disabled=move || loading.get() || info.get().is_some()
                    on:input=move |event| input.set(event_target_value(&event))
                    on:keydown=move |event: web_sys::KeyboardEvent| if event.key() == "Enter" { search.run(()); }/>
                {move || if info.get().is_some() {
                    view! { <button type="button" class="ui red button" on:click=move |_| clear.run(())>"×"</button> }.into_any()
                } else {
                    view! { <button type="button" class="ui button" disabled=move || loading.get() on:click=move |_| search.run(())>"Search"</button> }.into_any()
                }}
            </section>
            {move || if loading.get() {
                view! { <div class="ui active centered inline loader"></div> }.into_any()
            } else if let Some(problem) = error.get() {
                view! { <p role="alert">{problem}</p> }.into_any()
            } else if let (Some(info), Some(status), Some(endpoint)) = (info.get(), status.get(), endpoint.get()) {
                let presence = match status.presence { UserPresence::Online => "Online", UserPresence::Away => "Away", _ => "Offline" };
                let color = match status.presence { UserPresence::Online => "green", UserPresence::Away => "yellow", _ => "grey" };
                let picture = info.picture.clone().unwrap_or_default();
                let name = username.get();
                let href = format!("{}/browse?username={}", config::url_base().trim_end_matches('/'), util::encode_uri_component(&name));
                view! { <section class="ui raised segment users-user"><div class="ui items"><article class="item">
                    {if info.has_picture { view! { <img class="ui small image" alt="User picture" src=format!("data:image;base64,{}", picture)/> }.into_any() }
                     else { view! { <div class="users-picture-placeholder"><span class="ui big icon">"▧"</span></div> }.into_any() }}
                    <div class="content"><A attr:class="header" href=href>
                        <span class=format!("ui {} empty circular label", color)></span>{name}
                    </A>
                    <div class="meta">{format!("Presence: {presence}; Free upload slot: {}; Total upload slots: {}; Queue length: {}; IP address: {}, Port: {}", info.has_free_upload_slot, info.upload_slots, info.queue_length, endpoint.address, endpoint.port)}</div>
                    <div class="description">{info.description.unwrap_or_else(|| "No user info.".to_owned())}</div></div>
                </article></div></section> }.into_any()
            } else {
                view! { <section class="ui placeholder segment"><div class="ui icon header">No user info to display</div></section> }.into_any()
            }}
        </div>
    }
}

fn fetch_user(
    username: String,
    ticket: u64,
    info: RwSignal<Option<Info>>,
    status: RwSignal<Option<Status>>,
    endpoint: RwSignal<Option<Endpoint>>,
    loading: RwSignal<bool>,
    error: RwSignal<Option<String>>,
    generation: RwSignal<u64>,
) {
    leptos::task::spawn_local(async move {
        let encoded = util::encode_uri_component(&username);
        let user_info = api::get::<Info>(&format!("/users/{encoded}/info")).await;
        let user_status = api::get::<Status>(&format!("/users/{encoded}/status")).await;
        let user_endpoint = api::get::<Endpoint>(&format!("/users/{encoded}/endpoint")).await;
        if generation.try_get_untracked() != Some(ticket) {
            return;
        }
        match (user_info, user_status, user_endpoint) {
            (Ok(user_info), Ok(user_status), Ok(user_endpoint)) => {
                info.set(Some(user_info));
                status.set(Some(user_status));
                endpoint.set(Some(user_endpoint));
            }
            (a, b, c) => error.set(Some(
                a.err()
                    .or_else(|| b.err())
                    .or_else(|| c.err())
                    .unwrap()
                    .to_string(),
            )),
        }
        loading.set(false);
    });
}
