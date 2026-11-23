use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use leptos_router::{
    NavigateOptions,
    components::{A, Redirect, Route, Router, Routes},
    hooks::use_location,
    path,
};
use slskr_types::{
    application::State, config::Config, server::ConnectionState, telemetry::CurrentMetrics,
    transfers::TransferDirection,
};
use wasm_bindgen::{JsCast, closure::Closure};

use super::{
    app_footer::AppFooter, browse::Browse, chat::Chat, dashboard::Dashboard, login_form::LoginForm,
    rooms::Rooms, search::Searches, system::System, transfers::Transfers, users::Users,
};
use crate::{
    config,
    services::{
        api,
        events::{Event, Events, Status, Topic},
    },
    ui::{Modal, ModalAction},
};

#[derive(Clone, Copy)]
pub struct AppContext {
    pub state: RwSignal<State>,
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
    /// No web password is set: first-run setup comes first.
    SetupRequired,
    Authenticated {
        passthrough: bool,
    },
    Error(String),
}

/// True while the session cookie (or passthrough) authenticates; false on 401.
async fn check_token() -> Result<bool, api::ApiError> {
    match api::get_text("/session").await {
        Ok(_) => Ok(true),
        Err(error) if error.status == 401 => Ok(false),
        Err(error) => Err(error),
    }
}

async fn check_session() -> Result<Session, api::ApiError> {
    let auth = api::get::<slskr_types::session::AuthInfo>("/auth").await?;
    if auth.setup_required {
        return Ok(Session::SetupRequired);
    }
    let passthrough = !auth.enabled;
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

/// Placeholder until the setup wizard exists: the daemon has no web password yet.
#[component]
fn SetupRequired(retry: Callback<()>) -> impl IntoView {
    let setup = format!("{}/setup", config::url_base());
    view! {
        <div class="ui warning message view" role="alert">
            <div class="header">"Setup required — open "{setup.clone()}</div>
            <p>
                "No web password is set yet. Complete first-run setup at "
                <a href=setup.clone()>{setup.clone()}</a>
                "; from another machine you need the setup code printed in the daemon's log."
            </p>
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
        // Unscoped: the signed-in views are disposed once the session is cleared.
        leptos::task::spawn_local(async move {
            if let Err(error) = api::delete::<()>("/session").await {
                leptos::logging::warn!("Could not sign out: {error}");
            }
            session.set(Session::Login);
        });
    });
    move || match session.get() {
        Session::Checking => loading().into_any(),
        Session::Login => view! { <LoginForm on_login=retry/> }.into_any(),
        Session::SetupRequired => view! { <SetupRequired retry/> }.into_any(),
        Session::Error(message) => view! { <ConnectionError message retry/> }.into_any(),
        Session::Authenticated { passthrough } => view! {
            <AuthenticatedApp dark=dark passthrough=passthrough logout=logout retry=retry/>
        }
        .into_any(),
    }
}

#[component]
fn AuthenticatedApp(
    dark: RwSignal<bool>,
    passthrough: bool,
    logout: Callback<()>,
    retry: Callback<()>,
) -> impl IntoView {
    let state = RwSignal::new(State::default());
    let options = RwSignal::new(Config::default());
    let metrics = RwSignal::new(CurrentMetrics::default());
    // The live configuration is loaded (and gates readiness) but no view reads it yet.
    provide_context(AppContext { state, dark });

    let events = Events::start();
    let loaded = [
        RwSignal::new(false),
        RwSignal::new(false),
        RwSignal::new(false),
    ];
    let [state_loaded, options_loaded, metrics_loaded] = loaded;
    events.on(Topic::State, move |event| {
        if let Event::State(update) = event {
            state.try_set((**update).clone());
            state_loaded.try_set(true);
        }
    });
    events.on(Topic::Settings, move |event| {
        if let Event::Settings(update) = event {
            options.try_set((**update).clone());
            options_loaded.try_set(true);
        }
    });
    events.on(Topic::Metrics, move |event| {
        if let Event::Metrics(update) = event {
            metrics.try_set(update.clone());
            metrics_loaded.try_set(true);
        }
    });
    let status = events.status();
    // The session ended while connected: show the sign-in form.
    Effect::new(move |_| {
        if status.get() == Status::Unauthorized {
            retry.run(());
        }
    });
    let location = use_location();
    Effect::new(move |_| {
        location.pathname.track();
        location.search.track();
        let connected = status.get() == Status::Open;
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
    let ready = Memo::new(move |_| loaded.iter().all(|loaded| loaded.get()));

    view! {
        {move || if status.get() == Status::Reconnecting {
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

#[component]
fn HomeRedirect() -> impl IntoView {
    view! { <Redirect path="/dashboard" options=NavigateOptions { replace: true, ..Default::default() }/> }
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
                            <div class="view"><Dashboard/></div>
                        }/>
                        <Route path=path!("/searches/:id?") view=|| view! {
                            <div class="view"><Searches/></div>
                        }/>
                        <Route path=path!("/downloads") view=|| view! {
                            <div class="view"><Transfers direction=TransferDirection::Download/></div>
                        }/>
                        <Route path=path!("/uploads") view=|| view! {
                            <div class="view"><Transfers direction=TransferDirection::Upload/></div>
                        }/>
                        <Route path=path!("/rooms") view=|| view! { <Rooms/> }/>
                        <Route path=path!("/chat") view=|| view! { <Chat/> }/>
                        <Route path=path!("/users") view=|| view! { <Users/> }/>
                        <Route path=path!("/browse") view=|| view! { <Browse/> }/>
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
    let base = config::url_base().trim_end_matches('/').to_owned();
    let href = format!("{base}{path}");
    let location = leptos_router::hooks::use_location();
    // The vendored CSS styles `.active.item`; `<A>` alone only sets `aria-current`.
    let class = move || {
        location.pathname.with(|current| {
            let current = current
                .strip_prefix(base.as_str())
                .filter(|rest| rest.is_empty() || rest.starts_with('/'))
                .unwrap_or(current);
            let active = current == path
                || current
                    .strip_prefix(path)
                    .is_some_and(|rest| rest.starts_with('/'));
            if active { "active item" } else { "item" }
        })
    };
    view! { <A href=href attr:class=class><i class=icon aria-hidden="true"></i>{label}</A> }
}

#[component]
fn Navigation(passthrough: bool, logout: Callback<()>) -> impl IntoView {
    let AppContext { state, dark, .. } = expect_context();
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
            <NavLink path="/dashboard" icon="chart bar icon" label="Dashboard"/>
            <NavLink path="/searches" icon="search icon" label="Search"/>
            <NavLink path="/downloads" icon="download icon" label="Downloads"/>
            <NavLink path="/uploads" icon="upload icon" label="Uploads"/>
            <NavLink path="/rooms" icon="comments icon" label="Rooms"/>
            <NavLink path="/chat" icon="comment icon" label="Chat"/>
            <NavLink path="/users" icon="users icon" label="Users"/>
            <NavLink path="/browse" icon="folder open icon" label="Browse"/>
            <div class="right menu">
                <button type="button" class="item" aria-pressed=move || dark.get().to_string() on:click=toggle_theme>
                    <i class="adjust icon" aria-hidden="true"></i>"Theme"
                </button>
                <ConnectButton state=state/>
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
fn ConnectButton(state: RwSignal<State>) -> impl IntoView {
    let pending = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let connected = Memo::new(move |_| state.with(|state| state.server.state.is_connected()));
    let status = Memo::new(move |_| {
        state.with(|state| {
            if state.server.state.is_connected() {
                let icon = if state.pending_reconnect {
                    "yellow plug icon"
                } else {
                    "green plug icon"
                };
                (icon, "Connected")
            } else if matches!(
                state.server.state,
                ConnectionState::Connecting | ConnectionState::LoggingIn
            ) {
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
        let connected = connected.get_untracked();
        spawn_local_scoped_with_cancellation(async move {
            let result = if connected {
                api::post::<_, serde_json::Value>(
                    "/server/disconnect",
                    &serde_json::json!({"message": "client disconnected from web UI"}),
                )
                .await
            } else {
                api::post_empty::<serde_json::Value>("/server/connect").await
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
