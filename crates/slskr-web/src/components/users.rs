use super::browse::group_digits;
use crate::{
    config,
    services::{api, util},
    ui::{
        Alert, Avatar, Badge, Button, Card, EmptyState, Icon, Loading, PageHeader, Stat, StatusDot,
    },
};
use leptos::prelude::*;
use leptos_router::hooks::{use_navigate, use_url};
use slskr_types::users::{Endpoint, Info, Statistics, Status, UserPresence};

/// Everything known about the looked-up user. Only the status is required; the peer
/// answers the others, so each may fail on its own.
#[derive(Clone)]
struct Profile {
    username: String,
    status: Status,
    info: Result<Info, String>,
    statistics: Option<Statistics>,
    endpoint: Option<Endpoint>,
}

#[derive(Clone)]
enum Lookup {
    Idle,
    Loading(String),
    Failed(String, String),
    Found(Box<Profile>),
}

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
    let lookup = RwSignal::new(Lookup::Idle);
    let generation = RwSignal::new(0u64);
    let load = move |name: String| {
        input.set(name.clone());
        let ticket = generation.get_untracked().wrapping_add(1);
        generation.set(ticket);
        if name.is_empty() {
            lookup.set(Lookup::Idle);
        } else {
            lookup.set(Lookup::Loading(name.clone()));
            fetch_user(name, ticket, lookup, generation);
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
    let clear = Callback::new(move |_| navigate("/users", Default::default()));
    let loading = Signal::derive(move || lookup.with(|l| matches!(l, Lookup::Loading(_))));
    let has_user = Signal::derive(move || lookup.with(|l| !matches!(l, Lookup::Idle)));
    view! {
        <div class="users users-container stack">
            <PageHeader title="Users" description="Look up a Soulseek user to see their status, shares and profile."/>
            <Card title="Look up a user" class="users-form">
                <form class="input-group" on:submit=move |event| { event.prevent_default(); search.run(()); }>
                    <div class="input-icon grow">
                        <Icon name="search"/>
                        <input class="input users-input" type="search" placeholder="Username" aria-label="Username"
                            autocomplete="off" spellcheck="false"
                            prop:value=move || input.get()
                            on:input=move |event| input.set(event_target_value(&event))/>
                    </div>
                    <Button variant="primary" button_type="submit" icon="search" text="Look up" loading=loading/>
                    <Show when=move || has_user.get()>
                        <Button icon="x" label="Clear" on_click=clear/>
                    </Show>
                </form>
            </Card>
            {move || match lookup.get() {
                Lookup::Idle => view! {
                    <Card>
                        <EmptyState icon="users" title="No user selected"
                            description="Enter a username to see whether they are online, what they share and what they say about themselves."/>
                    </Card>
                }.into_any(),
                Lookup::Loading(name) => view! {
                    <Card><Loading label=format!("Looking up {name}…") show_label=true/></Card>
                }.into_any(),
                Lookup::Failed(name, message) => view! {
                    <Alert variant="danger" title=format!("Could not look up {name}") class="users-error">
                        <p>{message}</p>
                        <p class="muted">"Check the spelling and that slskr is connected to the Soulseek network."</p>
                        <div class="users-error__actions">
                            <Button size="sm" icon="rotate-cw" text="Try again" on_click=Callback::new(move |_| load(name.clone()))/>
                        </div>
                    </Alert>
                }.into_any(),
                Lookup::Found(profile) => view! { <UserProfile profile=*profile/> }.into_any(),
            }}
        </div>
    }
}

#[component]
fn UserProfile(profile: Profile) -> impl IntoView {
    let Profile {
        username,
        status,
        info,
        statistics,
        endpoint,
    } = profile;
    let base = config::url_base().trim_end_matches('/').to_owned();
    let encoded = util::encode_uri_component(&username);
    let browse_href = format!("{base}/browse?username={encoded}");
    let chat_href = format!("{base}/chat?username={encoded}");
    let (presence, dot, badge) = match status.presence {
        UserPresence::Online => ("Online", "online", "success"),
        UserPresence::Away => ("Away", "away", "warning"),
        UserPresence::Offline => ("Offline", "offline", "neutral"),
    };
    let dash = || "—".to_owned();
    let speed = statistics.map_or_else(dash, |s| util::format_speed(f64::from(s.average_speed)));
    let files = statistics.map_or_else(dash, |s| group_digits(s.file_count));
    let folders = statistics.map_or_else(dash, |s| group_digits(s.directory_count));
    let (info, info_error) = match info {
        Ok(info) => (Some(info), None),
        Err(error) => (None, Some(error)),
    };
    let slots = info.as_ref().map_or_else(dash, |info| {
        if info.has_free_upload_slot {
            "Available"
        } else {
            "None free"
        }
        .to_owned()
    });
    let slots_hint = info
        .as_ref()
        .map(|info| format!("{} slots in total", info.upload_slots));
    let queue = info
        .as_ref()
        .map_or_else(dash, |info| group_digits(info.queue_length));
    let description = info
        .as_ref()
        .and_then(|info| info.description.clone())
        .filter(|text| !text.trim().is_empty());
    let picture = info
        .as_ref()
        .and_then(|info| info.picture.clone())
        .filter(|picture| !picture.is_empty());
    let free_slot = info.as_ref().map(|info| info.has_free_upload_slot);
    view! {
        <section class="card users-profile" aria-labelledby="users-profile-name">
            <div class="card__header">
                <div class="users-profile__identity">
                    <Avatar name=username.clone() size="lg"/>
                    <div class="stack stack--xs users-profile__name">
                        <h2 class="card__title break-all" id="users-profile-name">{username.clone()}</h2>
                        <div class="cluster">
                            <Badge variant=badge class="users-presence">
                                <StatusDot status=dot/>{presence}
                            </Badge>
                            {status.is_privileged.then(|| view! { <Badge variant="purple" icon="star" text="Privileged"/> })}
                            {free_slot.map(|free| if free {
                                view! { <Badge variant="primary" text="Free upload slot"/> }.into_any()
                            } else {
                                view! { <Badge text="Upload slots busy"/> }.into_any()
                            })}
                        </div>
                    </div>
                </div>
                <div class="card__actions">
                    <Button icon="message-square" text="Send message" href=chat_href/>
                    <Button variant="primary" icon="folder-open" text="Browse shares" href=browse_href/>
                </div>
            </div>
            <div class="card__content stack">
                <div class="stats">
                    <Stat label="Upload speed" value=speed/>
                    <Stat label="Shared files" value=files/>
                    <Stat label="Shared folders" value=folders/>
                    {match slots_hint {
                        Some(hint) => view! { <Stat label="Upload slots" value=slots hint=hint/> }.into_any(),
                        None => view! { <Stat label="Upload slots" value=slots/> }.into_any(),
                    }}
                    <Stat label="Queue length" value=queue/>
                </div>
                {info_error.map(|error| view! {
                    <Alert variant="warning" title="No profile from this user">
                        <p>{error}</p>
                        <p class="muted">"The user may be offline or not accepting connections, so their description, picture and upload slots are unknown."</p>
                    </Alert>
                })}
                {info.is_some().then(|| view! {
                    <div class="users-profile__about">
                        <div class="users-profile__description">
                            <h3 class="section-title">"About"</h3>
                            {match description {
                                Some(text) => view! { <p class="users-description">{text}</p> }.into_any(),
                                None => view! { <p class="muted">"This user has not written a description."</p> }.into_any(),
                            }}
                        </div>
                        {picture.map(|picture| view! {
                            <figure class="users-picture">
                                <img alt=format!("Picture of {username}") src=format!("data:image;base64,{picture}")/>
                            </figure>
                        })}
                    </div>
                })}
                {endpoint.map(|endpoint| view! {
                    <dl class="dl users-endpoint">
                        <dt>"Address"</dt>
                        <dd class="mono">{format!("{}:{}", endpoint.address, endpoint.port)}</dd>
                    </dl>
                })}
            </div>
        </section>
    }
}

fn fetch_user(username: String, ticket: u64, lookup: RwSignal<Lookup>, generation: RwSignal<u64>) {
    leptos::task::spawn_local(async move {
        let encoded = util::encode_uri_component(&username);
        let info_path = format!("/users/{encoded}/info");
        let status_path = format!("/users/{encoded}/status");
        let statistics_path = format!("/users/{encoded}/statistics");
        let endpoint_path = format!("/users/{encoded}/endpoint");
        let (status, info, statistics, endpoint) = futures::join!(
            api::get::<Status>(&status_path),
            api::get::<Info>(&info_path),
            api::get::<Statistics>(&statistics_path),
            api::get::<Endpoint>(&endpoint_path),
        );
        if generation.try_get_untracked() != Some(ticket) {
            return;
        }
        let result = match status {
            Ok(status) => Lookup::Found(Box::new(Profile {
                username,
                status,
                info: info.map_err(|error| error.to_string()),
                statistics: statistics.ok(),
                endpoint: endpoint.ok(),
            })),
            Err(error) => Lookup::Failed(username, error.to_string()),
        };
        let _ = lookup.try_set(result);
    });
}
