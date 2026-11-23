mod results;

use std::collections::{BTreeMap, HashMap};

use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use leptos_router::{
    NavigateOptions,
    components::A,
    hooks::{use_navigate, use_params_map},
};
use slskr_types::search::{Search, SearchRequest, SearchResponse, SearchState};

use crate::{
    components::app::AppContext,
    config,
    services::{
        api,
        events::{Event, Status, Topic, use_events},
        util,
    },
};

#[derive(Default)]
struct Catalog {
    searches: HashMap<String, Search>,
    changed: HashMap<String, u64>,
    revision: u64,
}

impl Catalog {
    fn update(&mut self, id: String, search: Option<Search>) {
        self.revision += 1;
        self.changed.insert(id.clone(), self.revision);
        if let Some(search) = search {
            self.searches.insert(id, search);
        } else {
            self.searches.remove(&id);
        }
    }

    /// Replaces everything with a snapshot; every listed search counts as changed now.
    fn replace(&mut self, searches: Vec<Search>) {
        self.revision += 1;
        let revision = self.revision;
        self.changed = searches
            .iter()
            .map(|search| (search.id.clone(), revision))
            .collect();
        self.searches = searches
            .into_iter()
            .map(|search| (search.id.clone(), search))
            .collect();
    }
}

#[derive(Default)]
struct LiveResponses {
    id: String,
    responses: BTreeMap<String, SearchResponse>,
}

fn endpoint(id: &str) -> String {
    format!("/searches/{}", util::encode_uri_component(id))
}

fn complete(search: &Search) -> bool {
    search.state.is_complete()
}

#[component]
pub fn Searches() -> impl IntoView {
    let state = expect_context::<AppContext>().state;
    let connected = Signal::derive(move || state.with(|state| state.server.state.is_connected()));
    let params = use_params_map();
    let route = Memo::new(move |_| params.with(|params| params.get("id")));
    let navigate = StoredValue::new_local(use_navigate());
    let searches = RwSignal::new(Catalog::default());
    let live = RwSignal::new(LiveResponses::default());
    let loading = RwSignal::new(true);
    let connection_epoch = RwSignal::new(0u64);
    let query = RwSignal::new(String::new());
    let busy = RwSignal::new(false);
    let action_error = RwSignal::new(None::<String>);

    let events = use_events();
    let status = events.status();
    events.on(Topic::Searches, move |event| match event {
        Event::SearchList(list) => {
            // A snapshot (on every (re)connection) replaces what is known.
            let first = loading.get_untracked();
            searches.try_update(|catalog| catalog.replace(list.clone()));
            loading.try_set(false);
            if !first {
                connection_epoch.try_update(|epoch| *epoch += 1);
            }
        }
        Event::SearchCreated(search) | Event::SearchUpdated(search) => {
            let search = search.clone();
            searches.try_update(|catalog| catalog.update(search.id.clone(), Some(search)));
        }
        Event::SearchDeleted { id } => {
            searches.try_update(|catalog| catalog.update(id.clone(), None));
        }
        Event::SearchResponses {
            search_id,
            responses,
        } => {
            if !route.with_untracked(|id| id.as_ref() == Some(search_id)) {
                return;
            }
            live.try_update(|live| {
                if &live.id != search_id {
                    live.id = search_id.clone();
                    live.responses.clear();
                }
                for response in responses {
                    let response = response.clone();
                    live.responses.insert(response.username.clone(), response);
                }
            });
        }
        _ => {}
    });
    let live_error = move || {
        (status.get() == Status::Reconnecting).then_some("Disconnected from search updates")
    };

    // Requests run under this page's owner, not the clicked view's: a `search_deleted`
    // event can dispose the row or detail view before the DELETE answers, which would drop
    // the task and leave `busy` set.
    let owner = Owner::current().expect("Searches renders under an owner");
    let create_owner = owner.clone();
    let create = Callback::new(move |(text, open): (String, bool)| {
        let text = text.trim();
        if text.is_empty() || busy.get_untracked() || !connected.get_untracked() {
            return;
        }
        let request = SearchRequest {
            query: text.into(),
            ..Default::default()
        };
        busy.set(true);
        action_error.set(None);
        let revision = searches.with_untracked(|catalog| catalog.revision);
        let task = async move {
            match api::post::<_, Search>("/searches", &request).await {
                Ok(search) => {
                    let path = endpoint(&search.id);
                    searches.update(|catalog| {
                        if !catalog
                            .changed
                            .get(&search.id)
                            .is_some_and(|r| *r > revision)
                            || complete(&search)
                                && catalog
                                    .searches
                                    .get(&search.id)
                                    .is_some_and(|s| !complete(s))
                        {
                            catalog.update(search.id.clone(), Some(search));
                        }
                    });
                    query.set(String::new());
                    busy.set(false);
                    if open {
                        navigate.with_value(|navigate| navigate(&path, Default::default()));
                    }
                }
                Err(error) => {
                    action_error.set(Some(error.to_string()));
                    busy.set(false);
                }
            }
        };
        create_owner.with(|| spawn_local_scoped_with_cancellation(task));
    });
    let change = Callback::new(move |(id, remove): (String, bool)| {
        if busy.get_untracked() {
            return;
        }
        busy.set(true);
        action_error.set(None);
        let task = async move {
            let path = endpoint(&id);
            let result = if remove {
                api::request_text(gloo_net::http::Method::DELETE, &path, None).await
            } else {
                api::request_text(
                    gloo_net::http::Method::POST,
                    &format!("{path}/cancel"),
                    None,
                )
                .await
            };
            match result {
                Ok(_) if remove => {
                    searches.update(|catalog| catalog.update(id.clone(), None));
                    if route.with_untracked(|current| current.as_ref() == Some(&id)) {
                        navigate.with_value(|navigate| {
                            navigate(
                                "/searches",
                                NavigateOptions {
                                    replace: true,
                                    ..Default::default()
                                },
                            )
                        });
                    }
                }
                Ok(_) => {
                    // A successful stop requests cancellation; the completed state arrives as an event.
                    if let Ok(search) = api::get::<Search>(&path).await {
                        searches.update(|catalog| {
                            if catalog
                                .searches
                                .get(&id)
                                .is_some_and(|current| !complete(current))
                            {
                                catalog.update(id, Some(search));
                            }
                        });
                    }
                }
                Err(error) => action_error.set(Some(error.to_string())),
            }
            busy.set(false);
        };
        owner.with(|| spawn_local_scoped_with_cancellation(task));
    });

    view! {
        <div class="search-container">
            {move || action_error.get().map(|message| view! { <div class="ui negative message" role="alert">{message}</div> })}
            {move || live_error().map(|message| view! { <div class="ui warning message" role="status">"Live search updates unavailable: "{message}</div> })}
            {move || match route.get() {
                Some(id) => view! {
                    <SearchDetail id searches live loading connection_epoch busy connected create change/>
                }.into_any(),
                None => view! {
                    <form class="ui raised segment search-segment" aria-label="Search"
                        on:submit=move |event| { event.prevent_default(); create.run((query.get_untracked(), false)); }>
                        <div class="search-segment-icon"><i class="big search icon" aria-hidden="true"></i></div>
                        <div class="ui big action input search-input" class:loading=busy>
                            <input type="search" aria-label="Search phrase" data-lpignore="true" autofocus
                                disabled=move || busy.get() || !connected.get()
                                placeholder=move || if connected.get() { "Search phrase" } else { "Connect to server to perform a search" }
                                prop:value=query on:input=move |event| query.set(event_target_value(&event))/>
                            <button type="submit" class="ui icon button" title="Add search" aria-label="Add search"
                                disabled=move || busy.get() || !connected.get() || query.with(|text| text.trim().is_empty())>
                                <i class="plus icon" aria-hidden="true"></i>
                            </button>
                            <button type="button" class="ui icon button" title="Search and open results" aria-label="Search and open results"
                                disabled=move || busy.get() || !connected.get() || query.with(|text| text.trim().is_empty())
                                on:click=move |_| create.run((query.get_untracked(), true))>
                                <i class="search icon" aria-hidden="true"></i>
                            </button>
                        </div>
                    </form>
                    <SearchHistory searches loading busy change/>
                }.into_any(),
            }}
        </div>
    }
}

#[component]
fn SearchHistory(
    searches: RwSignal<Catalog>,
    loading: RwSignal<bool>,
    busy: RwSignal<bool>,
    change: Callback<(String, bool)>,
) -> impl IntoView {
    move || {
        let mut entries =
            searches.with(|catalog| catalog.searches.values().cloned().collect::<Vec<_>>());
        if entries.is_empty() {
            return if loading.get() {
                view! { <div class="ui raised segment"><div class="ui active centered inline loader" role="status" aria-label="Loading searches"></div></div> }.into_any()
            } else {
                view! { <div class="ui placeholder segment"><div class="ui icon header"><i class="search icon" aria-hidden="true"></i>"No searches to display"</div></div> }.into_any()
            };
        }
        entries.sort_unstable_by(|a, b| b.started_at.cmp(&a.started_at));
        view! {
            <div class="ui raised card search-list-card"><div class="content"><div class="search-list-wrapper">
                <table class="ui large unstackable table">
                    <thead><tr>
                        <th class="search-list-action"><i class="info circle icon" title="State"></i></th>
                        <th class="search-list-phrase">"Search"</th><th class="search-list-files">"Files"</th>
                        <th class="search-list-locked">"Locked"</th><th class="search-list-responses">"Responses"</th>
                        <th class="search-list-started">"Ended"</th><th class="search-list-action"></th>
                    </tr></thead>
                    <tbody>{entries.into_iter().map(|search| {
                        let done = complete(&search);
                        let path = format!("{}{}", config::url_base().trim_end_matches('/'), endpoint(&search.id));
                        let id = search.id;
                        let action = if done { "Delete search" } else { "Stop search" };
                        view! {
                            <tr>
                                <td>{status_icon(search.state)}</td><td class="search-list-phrase"><A href=path>{search.query}</A></td>
                                <td>{search.file_count}</td><td><i class="small yellow lock icon" aria-hidden="true"></i>{search.locked_file_count}</td>
                                <td>{search.response_count}</td><td>{search.ended_at.map_or_else(|| "—".into(), |date| util::format_date(&date.to_rfc3339()))}</td>
                                <td><button type="button" class="ui mini icon basic button" disabled=busy title=action aria-label=action
                                    on:click=move |_| change.run((id.clone(), done))><i class=action_icon(done) aria-hidden="true"></i></button></td>
                            </tr>
                        }
                    }).collect_view()}</tbody>
                </table>
            </div></div></div>
        }.into_any()
    }
}

#[component]
fn SearchDetail(
    id: String,
    searches: RwSignal<Catalog>,
    live: RwSignal<LiveResponses>,
    loading: RwSignal<bool>,
    connection_epoch: RwSignal<u64>,
    busy: RwSignal<bool>,
    connected: Signal<bool>,
    create: Callback<(String, bool)>,
    change: Callback<(String, bool)>,
) -> impl IntoView {
    let id = StoredValue::new(id);
    let search = Memo::new(move |_| {
        id.with_value(|id| searches.with(|catalog| catalog.searches.get(id).cloned()))
    });
    let known_completion = Memo::new(move |_| search.with(|search| search.as_ref().map(complete)));
    let done = Memo::new(move |_| known_completion.get() == Some(true));
    let saved = RwSignal::new(Vec::<SearchResponse>::new());
    let fetching = RwSignal::new(true);
    let error = RwSignal::new(None::<String>);
    let path = id.with_value(|id| format!("{}/responses", endpoint(id)));
    Effect::new(move |_| {
        let epoch = connection_epoch.get();
        let Some(complete) = known_completion.get() else {
            return;
        };
        let path = path.clone();
        fetching.set(true);
        spawn_local_scoped_with_cancellation(async move {
            let result = api::get::<Vec<SearchResponse>>(&path).await;
            if connection_epoch.get_untracked() != epoch
                || known_completion.get_untracked() != Some(complete)
            {
                return;
            }
            match result {
                Ok(rows) => {
                    saved.set(rows);
                    error.set(None);
                }
                Err(error_value) => error.set(Some(error_value.to_string())),
            }
            fetching.set(false);
        });
    });

    let status = move || {
        if done.get() {
            return if fetching.get() {
                "Loading results…".to_owned()
            } else {
                String::new()
            };
        }
        search.with(|search| {
            search.as_ref().map_or_else(
                || {
                    if loading.get() {
                        "Loading search…".into()
                    } else {
                        String::new()
                    }
                },
                |search| {
                    let (files, locked, users) = id.with_value(|id| {
                        live.with(|live| {
                            let responses = live.responses.values().filter(|_| &live.id == id);
                            responses.fold((0, 0, 0), |(files, locked, users), row| {
                                (
                                    files + row.files.len(),
                                    locked + row.locked_files.len(),
                                    users + 1,
                                )
                            })
                        })
                    });
                    let files = files.max(search.file_count as usize);
                    let locked = locked.max(search.locked_file_count as usize);
                    let users = users.max(search.response_count as usize);
                    let locked = if locked > 0 {
                        format!(" (plus {locked} locked)")
                    } else {
                        String::new()
                    };
                    format!("Found {files} files{locked} from {users} users")
                },
            )
        })
    };
    view! {
        <A href=format!("{}/searches", config::url_base().trim_end_matches('/')) attr:class="ui basic button"><i class="left arrow icon" aria-hidden="true"></i>"Search history"</A>
        {move || match search.get() {
            Some(search) => {
                let phrase = search.query.clone();
                view! {
                    <div class="ui raised segment search-detail-header-segment">
                        <h2 class="ui header">{status_icon(search.state)}{search.query}</h2>
                        <div class="search-detail-header-buttons">
                            <button type="button" class="ui button" disabled=move || busy.get() || !connected.get() || !done.get()
                                on:click=move |_| create.run((phrase.clone(), true))><i class="refresh icon" aria-hidden="true"></i>"Search Again"</button>
                            <button type="button" class="ui negative button" disabled=busy
                                on:click=move |_| change.run((id.get_value(), done.get_untracked()))>
                                <i class=move || action_icon(done.get()) aria-hidden="true"></i>{move || if done.get() { "Delete" } else { "Stop" }}
                            </button>
                        </div>
                    </div>
                }.into_any()
            }
            None if loading.get() => ().into_any(),
            None => view! { <div class="ui placeholder segment"><div class="ui icon header"><i class="search icon" aria-hidden="true"></i>"Search not found"</div></div> }.into_any(),
        }}
        {move || {
            let message = status();
            (!message.is_empty()).then(|| view! { <div class="ui raised segment" role="status"><span class="ui active inline loader" aria-hidden="true"></span>{message}</div> })
        }}
        {move || error.get().filter(|_| search.with(Option::is_some)).map(|error| view! { <div class="ui negative message" role="alert">{error}</div> })}
        <Show when=move || known_completion.with(Option::is_some)>
            <results::SearchResults id live saved connected done ready=Signal::derive(move || !fetching.get() && error.with(Option::is_none))/>
        </Show>
    }
}

fn action_icon(done: bool) -> &'static str {
    if done {
        "trash alternate icon"
    } else {
        "stop circle icon"
    }
}

fn status_icon(state: SearchState) -> impl IntoView {
    let class = match state {
        SearchState::Errored => "red x icon",
        SearchState::Cancelled => "green stop circle icon",
        SearchState::InProgress => "green loading circle notch icon",
        SearchState::Requested | SearchState::Queued => "time icon",
        _ => "green check icon",
    };
    view! { <i class=class title=state.label() aria-label=state.label()></i> }
}
