use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use leptos_router::{
    NavigateOptions,
    components::{A, Route, Router, Routes},
    hooks::{use_location, use_navigate},
    path,
};
use slskr_types::{
    application::State, config::Config, server::ConnectionState, telemetry::CurrentMetrics,
    transfers::TransferDirection,
};

use super::{
    browse::Browse,
    chat::Chat,
    dashboard::Dashboard,
    login_form::LoginForm,
    rooms::Rooms,
    search::Searches,
    setup::SetupWizard,
    system::{System, settings::SettingsPage},
    transfers::Transfers,
    users::Users,
};
use crate::{
    config,
    services::{
        api,
        events::{Event, Events, Status, Topic},
    },
    ui::{Alert, Brand, Icon, Loading, Modal, ModalAction, StatusDot, ThemeToggle, theme},
};

#[derive(Clone, Copy)]
pub struct AppContext {
    pub state: RwSignal<State>,
    /// Live transfer metrics (speeds, active and queued counts).
    pub metrics: RwSignal<CurrentMetrics>,
}

/// Lets pages end or re-check the session (e.g. after the web password changed).
#[derive(Clone, Copy)]
pub struct SessionControl {
    /// Checks the session again: shows the sign-in form when it ended.
    pub recheck: Callback<()>,
}

/// Session storage key of the page to open once first-run setup is complete.
const SETUP_RETURN_KEY: &str = "slskr-setup-return";

fn local_storage() -> Option<web_sys::Storage> {
    window().local_storage().ok().flatten()
}

#[derive(Clone, PartialEq)]
enum Session {
    Checking,
    Login,
    /// No web password is set: first-run setup comes first.
    SetupRequired,
    Authenticated {
        passthrough: bool,
        admin: bool,
    },
    Error(String),
}

/// True while the session cookie (or passthrough) authenticates; false on 401.
async fn check_token() -> Result<bool, api::ApiError> {
    Ok(session_info().await?.is_some())
}

/// The signed-in session; `None` on 401.
async fn session_info() -> Result<Option<slskr_types::session::Session>, api::ApiError> {
    match api::get::<slskr_types::session::Session>("/session").await {
        Ok(session) => Ok(Some(session)),
        Err(error) if error.status == 401 => Ok(None),
        Err(error) => Err(error),
    }
}

async fn check_session() -> Result<Session, api::ApiError> {
    let auth = api::get::<slskr_types::session::AuthInfo>("/auth").await?;
    if auth.setup_required {
        return Ok(Session::SetupRequired);
    }
    let passthrough = !auth.enabled;
    Ok(match session_info().await? {
        Some(session) => Session::Authenticated {
            passthrough,
            admin: passthrough || session.role == slskr_types::session::Role::Admin,
        },
        None => Session::Login,
    })
}

fn loading() -> impl IntoView {
    view! { <Loading page=true label="Connecting to slskr"/> }
}

/// The centered one-card layout of the sign-in, setup and error screens: the brand and the
/// theme toggle on top, the content below.
#[component]
pub(super) fn AuthLayout(
    /// Wider content (the setup wizard).
    #[prop(optional)]
    wide: bool,
    children: Children,
) -> impl IntoView {
    let class = if wide {
        "auth__card auth__card--wide"
    } else {
        "auth__card"
    };
    view! {
        <div class="auth">
            <header class="container app-header">
                <div class="app-header__bar">
                    <Brand/>
                    <div class="app-header__actions"><ThemeToggle/></div>
                </div>
            </header>
            <main class="container auth__main">
                <div class=class>{children()}</div>
            </main>
        </div>
    }
}

#[component]
fn ConnectionError(message: String, retry: Callback<()>) -> impl IntoView {
    view! {
        <AuthLayout>
            <div class="card">
                <div class="card__header">
                    <div>
                        <h1 class="card__title">"Could not connect to slskr"</h1>
                        <p class="card__description">"The daemon did not answer. Check that it is running, then retry."</p>
                    </div>
                </div>
                <div class="card__content stack">
                    <Alert variant="danger">{message}</Alert>
                    <button type="button" class="btn btn--primary btn--block" on:click=move |_| retry.run(())>
                        <Icon name="refresh-cw"/>"Retry"
                    </button>
                </div>
            </div>
        </AuthLayout>
    }
}

fn session_storage() -> Option<web_sys::Storage> {
    window().session_storage().ok().flatten()
}

/// First-run setup: shows the wizard at `/setup`, remembering the page that was opened so
/// it opens once setup is complete.
#[component]
fn SetupGate(on_done: Callback<()>) -> impl IntoView {
    let navigate = use_navigate();
    let location = use_location();
    let base = config::url_base().trim_end_matches('/').to_owned();
    let path = location.pathname.get_untracked();
    let relative = path
        .strip_prefix(base.as_str())
        .unwrap_or(&path)
        .trim_end_matches('/')
        .to_owned();
    if relative != "/setup" {
        if !relative.is_empty()
            && let Some(storage) = session_storage()
        {
            let target = format!("{relative}{}", location.search.get_untracked());
            let target = if target.starts_with('?') {
                format!("/{target}")
            } else {
                target
            };
            let _ = storage.set_item(SETUP_RETURN_KEY, &target);
        }
        navigate(
            "/setup",
            NavigateOptions {
                replace: true,
                ..Default::default()
            },
        );
    }
    let done = Callback::new(move |()| {
        let target = session_storage()
            .and_then(|storage| {
                let target = storage.get_item(SETUP_RETURN_KEY).ok().flatten();
                let _ = storage.remove_item(SETUP_RETURN_KEY);
                target
            })
            .filter(|target| target.starts_with('/') && !target.starts_with("/setup"))
            .unwrap_or_else(|| "/overview".to_owned());
        navigate(
            &target,
            NavigateOptions {
                replace: true,
                ..Default::default()
            },
        );
        on_done.run(());
    });
    view! { <SetupWizard on_done=done/> }
}

#[component]
fn SessionView() -> impl IntoView {
    let session = RwSignal::new(Session::Checking);
    let retry = Callback::new(move |()| session.set(Session::Checking));
    provide_context(SessionControl { recheck: retry });
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
        Session::SetupRequired => view! { <SetupGate on_done=retry/> }.into_any(),
        Session::Error(message) => view! { <ConnectionError message retry/> }.into_any(),
        Session::Authenticated { passthrough, admin } => view! {
            <AuthenticatedApp passthrough=passthrough admin=admin logout=logout retry=retry/>
        }
        .into_any(),
    }
}

#[component]
fn AuthenticatedApp(
    passthrough: bool,
    admin: bool,
    logout: Callback<()>,
    retry: Callback<()>,
) -> impl IntoView {
    let state = RwSignal::new(State::default());
    let options = RwSignal::new(Config::default());
    let metrics = RwSignal::new(CurrentMetrics::default());
    // The live configuration gates readiness; the onboarding banner reads it.
    provide_context(AppContext { state, metrics });
    provide_context(super::dashboard::ReportCache::new());

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
            view! {
                <div class="container reconnect-banner">
                    <Alert variant="warning" class="reconnect-alert">{move || if ready.get() { "Lost connection to slskr. Reconnecting…" }
                        else { "Could not open the live connection to slskr. Retrying…" }}</Alert>
                </div>
            }.into_any()
        } else if !ready.get() {
            loading().into_any()
        } else {
            ().into_any()
        }}
        <Show when=move || ready.get()>
            <AppShell options=options passthrough=passthrough admin=admin logout=logout/>
        </Show>
    }
}

const ONBOARDING_DISMISSED_KEY: &str = "slskr-dismiss-soulseek-onboarding";

/// "Connect your Soulseek account" while no Soulseek credentials are set (and they are not
/// pinned), until dismissed.
#[component]
fn SoulseekOnboarding(options: RwSignal<Config>) -> impl IntoView {
    let dismissed = RwSignal::new(
        local_storage()
            .and_then(|storage| storage.get_item(ONBOARDING_DISMISSED_KEY).ok().flatten())
            .is_some(),
    );
    let pinned = RwSignal::new(true);
    spawn_local_scoped_with_cancellation(async move {
        match api::get::<slskr_types::setup::SetupStatus>("/setup").await {
            Ok(status) => pinned.set(status.soulseek_pinned),
            Err(error) => leptos::logging::warn!("Could not read the setup status: {error}"),
        }
    });
    // The live configuration has secrets masked, so a set password is non-empty.
    let configured = Memo::new(move |_| {
        options.with(|c| !c.soulseek.username.is_empty() && !c.soulseek.password.is_empty())
    });
    let dismiss = move |_| {
        if let Some(storage) = local_storage() {
            let _ = storage.set_item(ONBOARDING_DISMISSED_KEY, "1");
        }
        dismissed.set(true);
    };
    let base = config::url_base().trim_end_matches('/').to_owned();
    view! {
        <Show when=move || !dismissed.get() && !pinned.get() && !configured.get()>
            <div class="alert alert--info onboarding-banner" role="region" aria-label="Soulseek account">
                <Icon name="plug"/>
                <div class="alert__body">
                    <div class="alert__title">"Connect your Soulseek account"</div>
                    <p>
                        "slskr is not connected to the Soulseek network yet. "
                        <A href=format!("{base}/system/settings?section=soulseek") attr:class="onboarding-link">
                            "Add your Soulseek username and password in Settings"
                        </A>
                        "."
                    </p>
                </div>
                <button type="button" class="btn btn--ghost btn--icon btn--sm alert__dismiss onboarding-dismiss"
                    aria-label="Dismiss" title="Dismiss" on:click=dismiss>
                    <Icon name="x"/>
                </button>
            </div>
        </Show>
    }
}

#[component]
pub(super) fn RouteRedirect(#[prop(into)] path: String) -> impl IntoView {
    let navigate = use_navigate();
    // Route rendering can hold the URL read lock. Navigate after it returns.
    Effect::new(move |_| {
        navigate(
            &path,
            NavigateOptions {
                replace: true,
                ..Default::default()
            },
        );
    });
}

#[component]
fn HomeRedirect() -> impl IntoView {
    view! { <RouteRedirect path="/overview"/> }
}

#[component]
fn AppShell(
    options: RwSignal<Config>,
    passthrough: bool,
    admin: bool,
    logout: Callback<()>,
) -> impl IntoView {
    view! {
        <div class="app">
            <Navigation passthrough logout/>
            <main class="container app-main app-content">
                {admin.then(|| view! { <SoulseekOnboarding options/> })}
                <Routes fallback=HomeRedirect>
                    <Route path=path!("/overview") view=Dashboard/>
                    // The page's former name.
                    <Route path=path!("/dashboard") view=HomeRedirect/>
                    <Route path=path!("/searches/:id?") view=Searches/>
                    <Route path=path!("/downloads") view=|| view! {
                        <Transfers direction=TransferDirection::Download/>
                    }/>
                    <Route path=path!("/uploads") view=|| view! {
                        <Transfers direction=TransferDirection::Upload/>
                    }/>
                    <Route path=path!("/rooms") view=Rooms/>
                    <Route path=path!("/chat") view=Chat/>
                    <Route path=path!("/users") view=Users/>
                    <Route path=path!("/browse") view=Browse/>
                    <Route path=path!("/settings/:category?") view=SettingsPage/>
                    // Settings used to be a System tab; keep `?section=` links working.
                    <Route path=path!("/system/settings") view=|| {
                        let search = use_location().search.get_untracked();
                        let query = if search.is_empty() || search.starts_with('?') { search } else { format!("?{search}") };
                        view! { <RouteRedirect path=format!("/settings{query}")/> }
                    }/>
                    <Route path=path!("/system/files/:root") view=System/>
                    <Route path=path!("/system/:tab?") view=System/>
                </Routes>
            </main>
        </div>
    }
}

/// Whether `path` (an app path such as `/system`) is the current page or one below it.
fn is_current(current: &str, base: &str, path: &str) -> bool {
    let current = current
        .strip_prefix(base)
        .filter(|rest| rest.is_empty() || rest.starts_with('/'))
        .unwrap_or(current);
    current == path
        || current
            .strip_prefix(path)
            .is_some_and(|rest| rest.starts_with('/'))
}

#[component]
fn NavLink(path: &'static str, icon: &'static str, label: &'static str) -> impl IntoView {
    let base = config::url_base().trim_end_matches('/').to_owned();
    let href = format!("{base}{path}");
    let location = use_location();
    let active = Memo::new(move |_| {
        location.pathname.with(|current| {
            is_current(current, &base, path)
                || (path == "/chat" && is_current(current, &base, "/rooms"))
        })
    });
    // `<A>` sets `aria-current` itself; the class keeps the active look independent of it.
    view! {
        <A href=href attr:class=move || if active.get() { "tabs__tab is-active" } else { "tabs__tab" }>
            <Icon name=icon/>
            <span>{label}</span>
        </A>
    }
}

#[component]
fn Navigation(passthrough: bool, logout: Callback<()>) -> impl IntoView {
    let AppContext { state, .. } = expect_context();
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
    let base = config::url_base().trim_end_matches('/').to_owned();
    let location = use_location();
    let settings_active = {
        let base = base.clone();
        Memo::new(move |_| {
            location
                .pathname
                .with(|current| is_current(current, &base, "/settings"))
        })
    };
    let info_href = format!("{base}/system/info");
    let settings_href = format!("{base}/settings");
    view! {
        <header class="container app-header">
            <div class="app-header__bar">
                <Brand href=format!("{base}/overview")/>
                <div class="app-header__actions">
                    <Show when=move || canary.get()>
                        <span class="badge badge--amber hide-sm"><Icon name="flask-conical"/>"Canary"</span>
                    </Show>
                    <Show when=move || pending.get()>
                        <A href=info_href.clone() attr:class="btn btn--sm pending-action"
                            attr:title="Pending action" attr:aria-label="Pending action">
                            <Icon name="circle-alert" class="text-warning"/>
                            <span class="hide-sm">"Pending action"</span>
                        </A>
                    </Show>
                    {move || version.get().map(|(current, latest)| view! {
                        <Modal size="sm" title="New version"
                            trigger=|| view! {
                                <button type="button" class="btn btn--sm" title="New version" aria-label="New version">
                                    <Icon name="megaphone" class="text-warning"/>
                                    <span class="hide-sm">"New version"</span>
                                </button>
                            }
                            content=format!("You are running {current}. Version {latest} is available.")
                            actions=vec![ModalAction::new("Close")]/>
                    })}
                    <ConnectButton state=state/>
                    <ThemeToggle/>
                    <A href=settings_href attr:class="btn btn--icon settings-link"
                        attr:aria-label="Settings" attr:title="Settings"
                        attr:aria-current=move || settings_active.get().then_some("page")>
                        <Icon name="settings"/>
                    </A>
                    <Show when=move || !passthrough>
                        <Modal size="sm" title="Log out?"
                            trigger=|| view! { <button type="button" class="btn logout-button">"Logout"</button> }
                            content="You will need your username and password to sign in again."
                            actions=vec![
                                ModalAction::new("Cancel"),
                                ModalAction::new("Log out").negative().on_click(move || logout.run(())),
                            ]/>
                    </Show>
                </div>
            </div>
            <div class="app-nav-row">
                <nav class="tabs app-nav navigation" aria-label="Main navigation">
                    <NavLink path="/overview" icon="gauge" label="Overview"/>
                    <NavLink path="/searches" icon="search" label="Search"/>
                    <NavLink path="/downloads" icon="download" label="Downloads"/>
                    <NavLink path="/uploads" icon="upload" label="Uploads"/>
                    <NavLink path="/browse" icon="folder-open" label="Browse"/>
                    <NavLink path="/users" icon="users" label="Users"/>
                    <NavLink path="/chat" icon="messages-square" label="Chat"/>
                    <NavLink path="/system" icon="server" label="System"/>
                </nav>
            </div>
        </header>
    }
}

#[component]
fn ConnectButton(state: RwSignal<State>) -> impl IntoView {
    let pending = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let connected = Memo::new(move |_| state.with(|state| state.server.state.is_connected()));
    // (dot status, text, busy)
    let status = Memo::new(move |_| {
        state.with(|state| {
            if state.server.state.is_connected() {
                let dot = if state.pending_reconnect {
                    "away"
                } else {
                    "online"
                };
                (dot, "Connected", false)
            } else if matches!(
                state.server.state,
                ConnectionState::Connecting | ConnectionState::LoggingIn
            ) {
                ("away", "Connecting", true)
            } else if state.connection_watchdog.is_attempting_connection {
                ("away", "Waiting to Reconnect", false)
            } else {
                ("offline", "Disconnected", false)
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
    let hint = move || {
        let action = if connected.get() {
            "click to disconnect"
        } else {
            "click to connect"
        };
        format!("Soulseek: {} ({action})", status.get().1)
    };
    view! {
        <button type="button" class="btn connection" title=hint aria-label=hint
            disabled=move || pending.get() on:click=move |_| {
                error.set(None);
                pending.set(true);
            }>
            {move || {
                let (dot, _, busy) = status.get();
                if busy || pending.get() {
                    view! { <span class="spinner" aria-hidden="true"></span> }.into_any()
                } else {
                    view! { <StatusDot status=dot/> }.into_any()
                }
            }}
            <span class="connection__text">{move || status.get().1}</span>
        </button>
        {move || error.get().map(|message| view! {
            <span class="connection-error" role="alert">{message}</span>
        })}
    }
}

#[component]
pub fn App() -> impl IntoView {
    let base = config::url_base();
    theme::install();
    view! {
        <Router base=base.trim_end_matches('/').to_owned()>
            <SessionView/>
        </Router>
    }
}
