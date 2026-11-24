mod results;

use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
};

use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use leptos_router::{
    NavigateOptions,
    components::A,
    hooks::{use_navigate, use_params_map},
};
use slskr_types::search::{Search, SearchRequest, SearchResponse, SearchState};

use crate::{
    components::{app::AppContext, dashboard::time_ago},
    config,
    services::{
        api,
        events::{Event, Status, Topic, use_events},
        util,
    },
    ui::{Alert, Button, Card, EmptyState, Icon, Modal, ModalAction, PageHeader},
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
    /// Shared, so the results view can tell a replaced response from one it already
    /// grouped into folders.
    responses: BTreeMap<String, Arc<SearchResponse>>,
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
    let creating = RwSignal::new(false);
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
                    live.responses
                        .insert(response.username.clone(), Arc::new(response.clone()));
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
    let clear_owner = owner.clone();
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
        creating.set(true);
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
                    creating.set(false);
                    if open {
                        navigate.with_value(|navigate| navigate(&path, Default::default()));
                    }
                }
                Err(error) => {
                    action_error.set(Some(error.to_string()));
                    busy.set(false);
                    creating.set(false);
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
    // Deletes every finished search, one request after another (there is no bulk route).
    let clear_completed = Callback::new(move |()| {
        if busy.get_untracked() {
            return;
        }
        let ids = searches.with_untracked(|catalog| {
            catalog
                .searches
                .values()
                .filter(|search| complete(search))
                .map(|search| search.id.clone())
                .collect::<Vec<_>>()
        });
        if ids.is_empty() {
            return;
        }
        busy.set(true);
        action_error.set(None);
        let task = async move {
            let mut failed = 0_usize;
            for id in ids {
                match api::request_text(gloo_net::http::Method::DELETE, &endpoint(&id), None).await
                {
                    Ok(_) => searches.update(|catalog| catalog.update(id, None)),
                    Err(_) => failed += 1,
                }
            }
            if failed > 0 {
                action_error.set(Some(format!(
                    "{failed} completed {} could not be deleted.",
                    if failed == 1 { "search" } else { "searches" }
                )));
            }
            busy.set(false);
        };
        clear_owner.with(|| spawn_local_scoped_with_cancellation(task));
    });
    let completed = Memo::new(move |_| {
        searches.with(|catalog| catalog.searches.values().filter(|s| complete(s)).count())
    });
    let confirm_clear = RwSignal::new(false);

    view! {
        <div class="search-container searches stack">
            {move || action_error.get().map(|message| view! {
                <Alert variant="danger" on_dismiss=Callback::new(move |()| action_error.set(None))>{message}</Alert>
            })}
            {move || live_error().map(|message| view! {
                <Alert variant="warning" title="Live search updates unavailable">{message}</Alert>
            })}
            {move || match route.get() {
                Some(id) => view! {
                    <SearchDetail id searches live loading connection_epoch busy connected create change/>
                }.into_any(),
                None => view! {
                    <PageHeader title="Search"
                        actions=ViewFn::from(move || view! {
                            <Show when=move || { completed.get() > 0 }>
                                <Button icon="trash-2" text="Clear completed" disabled=busy
                                    on_click=Callback::new(move |_| confirm_clear.set(true))/>
                            </Show>
                        })/>
                    <Modal size="sm" title="Clear completed searches?" open=confirm_clear
                        on_close=Callback::new(move |()| confirm_clear.set(false))
                        actions=vec![
                            ModalAction::new("Cancel"),
                            ModalAction::new("Clear completed").negative().on_click(move || clear_completed.run(())),
                        ]>
                        <p>{move || format!("This deletes {} finished {} and their results. Running searches are kept.",
                            completed.get(), if completed.get() == 1 { "search" } else { "searches" })}</p>
                    </Modal>
                    <SearchBox query busy creating connected create/>
                    <SearchHistory searches loading busy change/>
                }.into_any(),
            }}
        </div>
    }
}

#[component]
fn SearchBox(
    query: RwSignal<String>,
    busy: RwSignal<bool>,
    creating: RwSignal<bool>,
    connected: Signal<bool>,
    create: Callback<(String, bool)>,
) -> impl IntoView {
    let blank = Signal::derive(move || query.with(|text| text.trim().is_empty()));
    let blocked = Signal::derive(move || busy.get() || !connected.get() || blank.get());
    let help = RwSignal::new(false);
    view! {
        <Card class="search-box">
            <form class="search-box__form" role="search" aria-label="Search"
                on:submit=move |event| { event.prevent_default(); create.run((query.get_untracked(), true)); }>
                <div class="input-icon search-box__field">
                    <Icon name="search" size="lg"/>
                    <input type="search" class="input" aria-label="Search phrase"
                        aria-describedby=move || (!connected.get()).then_some("search-box-offline")
                        data-lpignore="true" autocomplete="off" autofocus
                        disabled=move || busy.get() || !connected.get()
                        placeholder=move || if connected.get() { "Artist, album, track…" } else { "Connect to the server to search" }
                        prop:value=query on:input=move |event| query.set(event_target_value(&event))/>
                    <Button class="search-box__syntax" variant="ghost" size="sm" icon="circle-help"
                        label="Advanced search help" on_click=Callback::new(move |_| help.set(true))/>
                </div>
                <div class="search-box__actions">
                    <Button size="lg" icon="plus" label="Add search without opening it" disabled=blocked
                        on_click=Callback::new(move |_| create.run((query.get_untracked(), false)))/>
                    <Button size="lg" variant="primary" button_type="submit" icon="search" text="Search"
                        loading=creating disabled=blocked/>
                </div>
            </form>
            <Show when=move || !connected.get()>
                <p id="search-box-offline" class="help search-box__help text-warning">
                    <Icon name="wifi-off" size="xs"/> " Not connected to the Soulseek server. Connect from the header to search."
                </p>
            </Show>
        </Card>
        <Modal open=help on_close=Callback::new(move |()| help.set(false)) title="Advanced search" size="sm">
            <div class="stack stack--sm">
                <p><code>"massive attack"</code>" searches for both words."</p>
                <p><code>"massive attack -live"</code>" excludes results containing “live”."</p>
                <p><code>"*step"</code>" matches words ending in “step”, such as “dubstep”."</p>
            </div>
        </Modal>
    }
}

#[component]
fn SearchHistory(
    searches: RwSignal<Catalog>,
    loading: RwSignal<bool>,
    busy: RwSignal<bool>,
    change: Callback<(String, bool)>,
) -> impl IntoView {
    let base = StoredValue::new(config::url_base().trim_end_matches('/').to_owned());
    // Rows re-render only when their search changed (keyed by id and revision).
    let rows = Memo::new(move |_| {
        searches.with(|catalog| {
            let mut rows = catalog
                .searches
                .values()
                .map(|search| {
                    let revision = catalog.changed.get(&search.id).copied().unwrap_or_default();
                    (revision, search.clone())
                })
                .collect::<Vec<_>>();
            rows.sort_unstable_by_key(|(_, search)| std::cmp::Reverse(search.started_at));
            rows
        })
    });
    let description = move || {
        rows.with(|rows| {
            let count = rows.len();
            let running = rows.iter().filter(|(_, search)| !complete(search)).count();
            let mut text = format!("{count} {}", if count == 1 { "search" } else { "searches" });
            if running > 0 {
                text.push_str(&format!(", {running} running"));
            }
            text
        })
    };
    view! {
        <section class="card card--flush search-history" aria-labelledby="search-history-title">
            <div class="card__header">
                <div>
                    <h2 class="card__title" id="search-history-title">"Recent searches"</h2>
                    <p class="card__description">{description}</p>
                </div>
            </div>
            <div class="card__content">
                {move || if rows.with(Vec::is_empty) {
                    if loading.get() {
                        view! {
                            <div class="table-wrap" role="status" aria-label="Loading searches">
                                <table class="table"><tbody>
                                    {(0..3).map(|_| view! { <tr><td><span class="skeleton search-history__skeleton"></span></td></tr> }).collect_view()}
                                </tbody></table>
                            </div>
                        }.into_any()
                    } else {
                        view! {
                            <EmptyState icon="search" title="No searches yet"
                                description="Searches you run appear here, newest first, with their results."/>
                        }.into_any()
                    }
                } else {
                    view! {
                        <div class="table-wrap">
                            <table class="table search-history__table">
                                <thead><tr>
                                    <th class="shrink">"State"</th>
                                    <th>"Search"</th>
                                    <th class="num">"Files"</th>
                                    <th class="num hide-sm">"Locked"</th>
                                    <th class="num hide-sm">"Responses"</th>
                                    <th class="hide-sm">"Started"</th>
                                    <th class="actions"><span class="sr-only">"Actions"</span></th>
                                </tr></thead>
                                <tbody>
                                    <For each=move || rows.get() key=|(revision, search)| (search.id.clone(), *revision)
                                        children=move |(_, search)| {
                                            let done = complete(&search);
                                            let path = base.with_value(|base| format!("{base}{}", endpoint(&search.id)));
                                            let id = search.id.clone();
                                            let action = if done { "Delete search" } else { "Stop search" };
                                            let locked = search.locked_file_count;
                                            view! {
                                                <tr class="search-row">
                                                    <td class="shrink">{state_badge(search.state)}</td>
                                                    <td class="search-row__query"><A href=path attr:class="search-row__link">{search.query}</A></td>
                                                    <td class="num">{search.file_count}</td>
                                                    <td class="num hide-sm">
                                                        {if locked > 0 {
                                                            view! { <span class="search-row__locked" title="Locked files"><Icon name="lock" size="xs"/>{locked}</span> }.into_any()
                                                        } else {
                                                            view! { <span class="muted">"0"</span> }.into_any()
                                                        }}
                                                    </td>
                                                    <td class="num hide-sm">{search.response_count}</td>
                                                    <td class="muted hide-sm">{time_ago(search.started_at)}</td>
                                                    <td class="actions">
                                                        <Button variant="action" class=if done { "is-danger" } else { "" }
                                                            icon=action_icon(done) label=action disabled=busy
                                                            on_click=Callback::new(move |_| change.run((id.clone(), done)))/>
                                                    </td>
                                                </tr>
                                            }
                                        }/>
                                </tbody>
                            </table>
                        </div>
                    }.into_any()
                }}
            </div>
        </section>
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
    let saved = RwSignal::new(Vec::<Arc<SearchResponse>>::new());
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
                    saved.set(rows.into_iter().map(Arc::new).collect());
                    error.set(None);
                }
                Err(error_value) => error.set(Some(error_value.to_string())),
            }
            fetching.set(false);
        });
    });

    // "Found 12 files (plus 3 locked) from 4 users", counting live responses while running.
    let found = move || {
        search.with(|search| {
            search.as_ref().map(|search| {
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
                format!(
                    "Found {files} {}{locked} from {users} {}",
                    if files == 1 { "file" } else { "files" },
                    if users == 1 { "user" } else { "users" }
                )
            })
        })
    };
    let working = move || !done.get() || fetching.get();
    let history = StoredValue::new(format!(
        "{}/searches",
        config::url_base().trim_end_matches('/')
    ));
    view! {
        <div class="search-detail stack">
            <div>
                <A href=history.get_value() attr:class="btn btn--ghost btn--sm search-detail__back">
                    <Icon name="arrow-left"/>"Search history"
                </A>
            </div>
            {move || match search.get() {
                Some(current) => {
                    let phrase = current.query.clone();
                    view! {
                        <div class="page-header search-detail__header">
                            <div class="search-detail__heading">
                                <div class="search-detail__title-row">
                                    <h1 class="page-header__title search-detail__title">{current.query.clone()}</h1>
                                    {state_badge(current.state)}
                                </div>
                                <p class="page-header__description search-detail__status" role="status">
                                    {move || working().then(|| view! { <span class="spinner" aria-hidden="true"></span> })}
                                    {move || if done.get() && fetching.get() { "Loading results…".to_owned() } else { found().unwrap_or_default() }}
                                </p>
                            </div>
                            <div class="page-header__actions">
                                <Button icon="refresh-cw" text="Search again"
                                    disabled=Signal::derive(move || busy.get() || !connected.get() || !done.get())
                                    on_click=Callback::new(move |_| create.run((phrase.clone(), true)))/>
                                <Button variant="danger-outline" icon=Signal::derive(move || action_icon(done.get()).to_owned())
                                    text=Signal::derive(move || if done.get() { "Delete".to_owned() } else { "Stop".to_owned() })
                                    disabled=busy
                                    on_click=Callback::new(move |_| change.run((id.get_value(), done.get_untracked())))/>
                            </div>
                        </div>
                    }.into_any()
                }
                None if loading.get() => view! {
                    <div class="page-header" role="status" aria-label="Loading search">
                        <div class="stack stack--sm grow">
                            <span class="skeleton search-detail__skeleton-title"></span>
                            <span class="skeleton search-detail__skeleton-line"></span>
                        </div>
                    </div>
                }.into_any(),
                None => view! {
                    <Card>
                        <EmptyState icon="search" title="Search not found"
                            description="It may have been deleted. Start a new search from the search page.">
                            <A href=history.get_value() attr:class="btn btn--primary"><Icon name="search"/>"Go to search"</A>
                        </EmptyState>
                    </Card>
                }.into_any(),
            }}
            {move || error.get().filter(|_| search.with(Option::is_some)).map(|error| view! {
                <Alert variant="danger" title="Could not load the results">{error}</Alert>
            })}
            <Show when=move || known_completion.with(Option::is_some)>
                <results::SearchResults id live saved connected done ready=Signal::derive(move || !fetching.get() && error.with(Option::is_none))/>
            </Show>
        </div>
    }
}

fn action_icon(done: bool) -> &'static str {
    if done { "trash-2" } else { "circle-stop" }
}

/// The search state as a coloured badge.
fn state_badge(state: SearchState) -> impl IntoView {
    let (class, icon) = match state {
        SearchState::Errored => ("badge badge--danger", "circle-x"),
        SearchState::Cancelled => ("badge badge--warning", "circle-stop"),
        SearchState::InProgress => ("badge badge--primary", "loader-circle"),
        SearchState::Requested | SearchState::Queued => ("badge", "clock"),
        _ => ("badge badge--success", "check"),
    };
    let spin = state == SearchState::InProgress;
    view! {
        <span class=format!("{class} search-state") title=state.label()>
            <Icon name=icon spin=spin/><span class="search-state__label">{state.label()}</span>
        </span>
    }
}
