use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use leptos_router::{
    NavigateOptions,
    components::{A, Redirect, Route, Router, Routes},
    hooks::use_location,
    path,
};
use serde::de::DeserializeOwned;
use slskr_api::{
    application::{State, hub_methods as application_methods},
    options::Options,
    relay::{RelayClientState, RelayMode},
    telemetry::{CurrentMetrics, hub_methods as metrics_methods},
    transfers::TransferDirection,
};
use wasm_bindgen::{JsCast, closure::Closure};

use super::{
    app_footer::AppFooter, browse::Browse, chat::Chat, dashboard::Dashboard, login_form::LoginForm,
    rooms::Rooms, search::Searches, system::System, transfers::Transfers, users::Users,
};
use crate::{
    config::{self, TOKEN_PASSTHROUGH_VALUE},
    services::{
        api,
        hub::{self, HubConnection},
        token::{self, TokenStorage},
    },
    ui::{Modal, ModalAction},
};

#[derive(Clone, Copy)]
pub struct AppContext {
    pub state: RwSignal<State>,
    pub options: RwSignal<Options>,
    pub dark: RwSignal<bool>,
}

const THEME_KEY: &str = "slskd-theme";

fn local_storage() -> Option<web_sys::Storage> {
    window().local_storage().ok().flatten()
}

fn install_theme() -> RwSignal<bool> {
    let saved = local_storage().and_then(|storage| storage.get_item(THEME_KEY).ok().flatten());
    let system = window()
        .match_media("(prefers-color-scheme: dark)")
        .ok()
        .flatten();
    let dark = RwSignal::new(match saved.as_deref() {
        Some("dark") => true,
        Some("light") => false,
        _ => system
            .as_ref()
            .is_some_and(web_sys::MediaQueryList::matches),
    });
    if let Some(system) = system {
        let media = system.clone();
        let listener = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
            let saved =
                local_storage().and_then(|storage| storage.get_item(THEME_KEY).ok().flatten());
            if !matches!(saved.as_deref(), Some("dark" | "light")) {
                dark.try_set(media.matches());
            }
        });
        let callback = listener.as_ref().unchecked_ref();
        if system
            .add_event_listener_with_callback("change", callback)
            .is_ok()
        {
            let listener = StoredValue::new_local((system, listener));
            on_cleanup(move || {
                listener.with_value(|(system, listener)| {
                    let _ = system.remove_event_listener_with_callback(
                        "change",
                        listener.as_ref().unchecked_ref(),
                    );
                })
            });
        }
    }
    Effect::new(move |_| {
        if let Some(root) = document().document_element() {
            let _ = root.class_list().toggle_with_force("dark", dark.get());
        }
    });
    dark
}

#[derive(Clone, PartialEq)]
enum Session {
    Checking,
    Login,
    Authenticated { passthrough: bool },
    Error(String),
}

async fn check_token() -> Result<bool, api::ApiError> {
    match api::get_text("/session").await {
        Ok(_) => Ok(true),
        Err(error) if error.status == 401 => {
            token::clear_token();
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

async fn check_session() -> Result<Session, api::ApiError> {
    let passthrough = !api::get::<bool>("/session/enabled").await?;
    if passthrough {
        token::set_token(TokenStorage::Session, TOKEN_PASSTHROUGH_VALUE);
    }
    Ok(if check_token().await? {
        Session::Authenticated { passthrough }
    } else {
        Session::Login
    })
}

fn loading() -> impl IntoView {
    view! { <div class="ui active large loader" role="status" aria-label="Connecting to slskr"></div> }
}

#[component]
fn ConnectionError(message: String, retry: Callback<()>) -> impl IntoView {
    view! {
        <div class="ui negative message view" role="alert">
            <div class="header">"Could not connect to slskr"</div>
            <p>{message}</p>
            <button type="button" class="ui button" on:click=move |_| retry.run(())>"Retry"</button>
        </div>
    }
}

#[component]
fn SessionView(dark: RwSignal<bool>) -> impl IntoView {
    let session = RwSignal::new(Session::Checking);
    let retry = Callback::new(move |()| session.set(Session::Checking));
    Effect::new(move |_| {
        if session.get() == Session::Checking {
            spawn_local_scoped_with_cancellation(async move {
                let next = check_session()
                    .await
                    .unwrap_or_else(|error| Session::Error(error.to_string()));
                session.set(next);
            });
        }
    });
    let logout = Callback::new(move |()| {
        token::clear_token();
        session.set(Session::Login);
    });
    move || match session.get() {
        Session::Checking => loading().into_any(),
        Session::Login => view! { <LoginForm on_login=retry/> }.into_any(),
        Session::Error(message) => view! { <ConnectionError message retry/> }.into_any(),
        Session::Authenticated { passthrough } => view! {
            <AuthenticatedApp dark=dark passthrough=passthrough logout=logout retry=retry/>
        }
        .into_any(),
    }
}

#[derive(Clone, PartialEq)]
enum Connection {
    Starting,
    Connected,
    Reconnecting,
    Closed(String),
}

fn connect_hub(hub: HubConnection, status: RwSignal<Connection>, label: &'static str) {
    hub.on_reconnecting(move |_| {
        status.try_set(Connection::Reconnecting);
    });
    hub.on_reconnected(move |_| {
        status.try_set(Connection::Connected);
    });
    hub.on_close(move |error| {
        let detail = error.unwrap_or_else(|| "connection closed".into());
        status.try_set(Connection::Closed(format!("{label}: {detail}")));
    });
    // Keep a handle for owner cleanup, even if cancellation drops the startup future.
    let connection = StoredValue::new_local(hub.clone());
    on_cleanup(move || connection.with_value(HubConnection::stop));
    spawn_local_scoped_with_cancellation(async move {
        let next = match hub.start().await {
            Ok(()) => Connection::Connected,
            Err(error) => Connection::Closed(format!("{label}: {error}")),
        };
        status.try_set(next);
    });
}

fn watch_hub_value<T>(
    hub: &HubConnection,
    method: &'static str,
    endpoint: &'static str,
    connection: RwSignal<Connection>,
    value: RwSignal<T>,
) -> RwSignal<Option<Result<(), String>>>
where
    T: DeserializeOwned + Send + Sync + 'static,
{
    let revision = RwSignal::new(0u64);
    hub.on(method, move |args| {
        if let Some(payload) = args.into_iter().next() {
            match serde_json::from_value::<T>(payload) {
                Ok(update) => {
                    revision.try_update(|revision| *revision = revision.wrapping_add(1));
                    value.try_set(update);
                }
                Err(error) => leptos::logging::warn!("Invalid {method} hub update: {error}"),
            }
        }
    });
    let loaded = RwSignal::new(None);
    Effect::new(move |_| {
        if connection.get() == Connection::Connected {
            let before = revision.get_untracked();
            spawn_local_scoped_with_cancellation(async move {
                match api::get::<T>(endpoint).await {
                    Ok(snapshot) => {
                        // An update received during the request is newer than the HTTP snapshot.
                        if revision.get_untracked() == before {
                            value.set(snapshot);
                        }
                        loaded.set(Some(Ok(())));
                    }
                    Err(error) => loaded.set(Some(Err(format!("{endpoint}: {error}")))),
                }
            });
        }
    });
    loaded
}

#[component]
fn AuthenticatedApp(
    dark: RwSignal<bool>,
    passthrough: bool,
    logout: Callback<()>,
    retry: Callback<()>,
) -> impl IntoView {
    let state = RwSignal::new(State::default());
    let options = RwSignal::new(Options::default());
    let metrics = RwSignal::new(CurrentMetrics::default());
    provide_context(AppContext {
        state,
        options,
        dark,
    });

    let application = hub::create_application_hub();
    let telemetry = hub::create_metrics_hub();
    let app_connection = RwSignal::new(Connection::Starting);
    let metrics_connection = RwSignal::new(Connection::Starting);
    let snapshots = [
        watch_hub_value(
            &application,
            application_methods::STATE,
            "/application",
            app_connection,
            state,
        ),
        watch_hub_value(
            &application,
            application_methods::OPTIONS,
            "/options",
            app_connection,
            options,
        ),
        watch_hub_value(
            &telemetry,
            metrics_methods::UPDATE,
            "/telemetry/metrics/current",
            metrics_connection,
            metrics,
        ),
    ];
    connect_hub(application, app_connection, "Application hub");
    connect_hub(telemetry, metrics_connection, "Metrics hub");
    let location = use_location();
    Effect::new(move |_| {
        location.pathname.track();
        location.search.track();
        let connected = app_connection.get() == Connection::Connected;
        if !passthrough && connected {
            spawn_local_scoped_with_cancellation(async move {
                match check_token().await {
                    Ok(false) => logout.run(()),
                    Ok(true) => {}
                    Err(error) => leptos::logging::warn!("Could not check session: {error}"),
                }
            });
        }
    });
    let connections = [app_connection, metrics_connection];
    let ready = Memo::new(move |_| {
        snapshots
            .iter()
            .all(|snapshot| snapshot.with(|result| matches!(result, Some(Ok(())))))
    });
    let issue = Memo::new(move |_| {
        connections
            .iter()
            .find_map(|connection| {
                connection.with(|status| match status {
                    Connection::Closed(error) => Some(error.clone()),
                    _ => None,
                })
            })
            .or_else(|| {
                snapshots.iter().find_map(|snapshot| {
                    snapshot.with(|result| {
                        result
                            .as_ref()
                            .and_then(|result| result.as_ref().err())
                            .cloned()
                    })
                })
            })
    });
    let reconnecting = move || {
        connections
            .iter()
            .any(|connection| connection.with(|status| matches!(status, Connection::Reconnecting)))
    };

    view! {
        {move || if let Some(message) = issue.get() {
            view! { <ConnectionError message retry/> }.into_any()
        } else if reconnecting() {
            view! { <div class="ui warning message view" role="status">"Lost connection to slskr. Reconnecting…"</div> }.into_any()
        } else if !ready.get() {
            loading().into_any()
        } else {
            ().into_any()
        }}
        <Show when=move || ready.get()>
            <AppShell metrics=metrics passthrough=passthrough logout=logout/>
        </Show>
    }
}

fn agent_mode(state: RwSignal<State>) -> Memo<bool> {
    Memo::new(move |_| state.with(|state| state.relay.mode == RelayMode::Agent))
}

#[component]
fn HomeRedirect() -> impl IntoView {
    let state = expect_context::<AppContext>().state;
    let target = if state.with_untracked(|state| state.relay.mode == RelayMode::Agent) {
        "/system"
    } else {
        "/dashboard"
    };
    view! { <Redirect path=target options=NavigateOptions { replace: true, ..Default::default() }/> }
}

#[component]
fn NonAgent(children: ChildrenFn) -> impl IntoView {
    let agent = agent_mode(expect_context::<AppContext>().state);
    view! {
        <Show when=move || !agent.get() fallback=HomeRedirect>
            {children()}
        </Show>
    }
}

#[component]
fn AppShell(
    metrics: RwSignal<CurrentMetrics>,
    passthrough: bool,
    logout: Callback<()>,
) -> impl IntoView {
    let state = expect_context::<AppContext>().state;
    view! {
        <div class="ui pushable segment app">
            <Navigation passthrough logout/>
            <main class="pusher app-content">
                <div class="app-content-body">
                    <Routes fallback=HomeRedirect>
                        <Route path=path!("/dashboard") view=|| view! {
                            <NonAgent><div class="view"><Dashboard/></div></NonAgent>
                        }/>
                        <Route path=path!("/searches/:id?") view=|| view! {
                            <NonAgent><div class="view"><Searches/></div></NonAgent>
                        }/>
                        <Route path=path!("/downloads") view=|| view! {
                            <NonAgent><div class="view"><Transfers direction=TransferDirection::Download/></div></NonAgent>
                        }/>
                        <Route path=path!("/uploads") view=|| view! {
                            <NonAgent><div class="view"><Transfers direction=TransferDirection::Upload/></div></NonAgent>
                        }/>
                        <Route path=path!("/rooms") view=|| view! { <NonAgent><Rooms/></NonAgent> }/>
                        <Route path=path!("/chat") view=|| view! { <NonAgent><Chat/></NonAgent> }/>
                        <Route path=path!("/users") view=|| view! { <NonAgent><Users/></NonAgent> }/>
                        <Route path=path!("/browse") view=|| view! { <NonAgent><Browse/></NonAgent> }/>
                        <Route path=path!("/system/files/:root") view=System/>
                        <Route path=path!("/system/:tab?") view=System/>
                    </Routes>
                </div>
            </main>
        </div>
        <AppFooter state=state metrics=metrics/>
    }
}

#[component]
fn NavLink(path: &'static str, icon: &'static str, label: &'static str) -> impl IntoView {
    let href = format!("{}{path}", config::url_base().trim_end_matches('/'));
    view! { <A href=href attr:class="item"><i class=icon aria-hidden="true"></i>{label}</A> }
}

#[component]
fn Navigation(passthrough: bool, logout: Callback<()>) -> impl IntoView {
    let AppContext { state, dark, .. } = expect_context();
    let agent = agent_mode(state);
    let canary = Memo::new(move |_| state.with(|state| state.version.is_canary));
    let pending = Memo::new(move |_| {
        state.with(|state| {
            state.pending_reconnect || state.pending_restart || state.shares.scan_pending
        })
    });
    let version = Memo::new(move |_| {
        state.with(|state| {
            state.version.is_update_available.unwrap_or(false).then(|| {
                (
                    state.version.current.clone(),
                    state.version.latest.clone().unwrap_or_default(),
                )
            })
        })
    });
    let toggle_theme = move |_| {
        let next = !dark.get_untracked();
        if let Some(storage) = local_storage() {
            let _ = storage.set_item(THEME_KEY, if next { "dark" } else { "light" });
        }
        dark.set(next);
    };
    view! {
        <nav class="ui inverted labeled icon menu navigation" aria-label="Main navigation">
            <Show when=move || canary.get()>
                <span class="item"><i class="yellow flask icon" aria-hidden="true"></i>"Canary"</span>
            </Show>
            <Show when=move || agent.get() fallback=|| view! {
                <NavLink path="/dashboard" icon="chart bar icon" label="Dashboard"/>
                <NavLink path="/searches" icon="search icon" label="Search"/>
                <NavLink path="/downloads" icon="download icon" label="Downloads"/>
                <NavLink path="/uploads" icon="upload icon" label="Uploads"/>
                <NavLink path="/rooms" icon="comments icon" label="Rooms"/>
                <NavLink path="/chat" icon="comment icon" label="Chat"/>
                <NavLink path="/users" icon="users icon" label="Users"/>
                <NavLink path="/browse" icon="folder open icon" label="Browse"/>
            }>
                <span class="item"><i class="detective icon" aria-hidden="true"></i>"Agent Mode"</span>
            </Show>
            <div class="right menu">
                <button type="button" class="item" aria-pressed=move || dark.get().to_string() on:click=toggle_theme>
                    <i class="adjust icon" aria-hidden="true"></i>"Theme"
                </button>
                <ConnectButton state=state agent=agent/>
                <Show when=move || pending.get()>
                    <NavLink path="/system/info" icon="yellow exclamation circle icon" label="Pending Action"/>
                </Show>
                {move || version.get().map(|(current, latest)| view! {
                    <Modal size="mini" close_icon=true
                        trigger=|| view! {
                            <button type="button" class="item"><i class="yellow bullhorn icon" aria-hidden="true"></i>"New Version"</button>
                        }
                        header=|| view! { <div class="header">"New Version"</div> }
                        content=format!("You are running {current}. Version {latest} is available.")
                        actions=vec![ModalAction::new("Close")]/>
                })}
                <NavLink path="/system" icon="cogs icon" label="System"/>
                <Show when=move || !passthrough>
                    <Modal size="mini"
                        trigger=|| view! {
                            <button type="button" class="item"><i class="sign out icon" aria-hidden="true"></i>"Log Out"</button>
                        }
                        header=|| view! { <div class="header">"Confirm Log Out"</div> }
                        content="Are you sure you want to log out?"
                        actions=vec![
                            ModalAction::new("Cancel"),
                            ModalAction::new("Log Out").negative().on_click(move || logout.run(())),
                        ]/>
                </Show>
            </div>
        </nav>
    }
}

#[component]
fn ConnectButton(state: RwSignal<State>, agent: Memo<bool>) -> impl IntoView {
    let pending = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let connected = Memo::new(move |_| {
        state.with(|state| {
            if agent.get() {
                state.relay.controller.state == RelayClientState::Connected
            } else {
                state.server.is_connected
            }
        })
    });
    let status = Memo::new(move |_| {
        state.with(|state| {
            if agent.get() {
                match state.relay.controller.state {
                    RelayClientState::Connected => ("green plug icon", "Controller Connected"),
                    RelayClientState::Connecting => (
                        "yellow sync alternate loading icon",
                        "Controller Connecting",
                    ),
                    RelayClientState::Reconnecting => (
                        "yellow sync alternate loading icon",
                        "Controller Reconnecting",
                    ),
                    RelayClientState::Disconnected => ("grey plug icon", "Controller Disconnected"),
                }
            } else if state.server.is_connected {
                let icon = if state.pending_reconnect {
                    "yellow plug icon"
                } else {
                    "green plug icon"
                };
                (icon, "Connected")
            } else if state.server.is_connecting || state.server.is_logging_in {
                ("green sync alternate loading icon", "Connecting")
            } else if state.connection_watchdog.is_attempting_connection {
                ("yellow clock icon", "Waiting to Reconnect")
            } else {
                ("red plug icon", "Disconnected")
            }
        })
    });
    Effect::new(move |_| {
        if !pending.get() {
            return;
        }
        let agent = agent.get_untracked();
        let connected = connected.get_untracked();
        spawn_local_scoped_with_cancellation(async move {
            let result = match (agent, connected) {
                (true, true) => api::delete::<serde_json::Value>("/relay/agent").await,
                (true, false) => api::put_empty::<serde_json::Value>("/relay/agent").await,
                (false, true) => {
                    api::delete_with::<_, serde_json::Value>(
                        "/server",
                        "client disconnected from web UI",
                    )
                    .await
                }
                (false, false) => api::put_empty::<serde_json::Value>("/server").await,
            };
            if let Err(err) = result {
                error.set(Some(err.to_string()));
            }
            pending.set(false);
        });
    });
    view! {
        <button type="button" class="item" disabled=move || pending.get() on:click=move |_| {
            error.set(None);
            pending.set(true);
        }>
            <i class=move || status.get().0 aria-hidden="true"></i>
            {move || status.get().1}
        </button>
        {move || error.get().map(|message| view! { <span class="item" role="alert">{message}</span> })}
    }
}

#[component]
pub fn App() -> impl IntoView {
    let base = config::url_base();
    let dark = install_theme();
    view! {
        <Router base=base.trim_end_matches('/').to_owned()>
            <SessionView dark=dark/>
        </Router>
    }
}
