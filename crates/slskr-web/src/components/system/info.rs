//! System → Info: version, Soulseek connection, daemon addresses and paths, the
//! distributed network, and restart/shutdown.

use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use slskr_types::{application::State, config::Config, server::ConnectionState};

use super::format;
use crate::{
    components::app::AppContext,
    services::{api, util::format_speed},
    ui::{Alert, Badge, Card, Icon, Modal, ModalAction, StatusDot},
};

/// Badge variant and text for the server connection.
pub(super) fn connection_badge(state: ConnectionState) -> (&'static str, &'static str) {
    match state {
        ConnectionState::LoggedIn => ("success", "Connected"),
        ConnectionState::Connected => ("primary", "Connected, not logged in"),
        ConnectionState::Connecting => ("primary", "Connecting"),
        ConnectionState::LoggingIn => ("primary", "Logging in"),
        ConnectionState::Disconnecting => ("warning", "Disconnecting"),
        ConnectionState::Disconnected => ("danger", "Disconnected"),
    }
}

fn connection_dot(state: ConnectionState) -> &'static str {
    match state {
        ConnectionState::LoggedIn => "online",
        ConnectionState::Disconnected => "offline",
        _ => "away",
    }
}

fn or_dash(value: Option<String>) -> String {
    value
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "—".to_owned())
}

#[component]
pub fn Info() -> impl IntoView {
    let state = use_context::<AppContext>()
        .map(|c| c.state)
        .unwrap_or_default();
    let config = RwSignal::new(None::<Config>);
    spawn_local_scoped_with_cancellation(async move {
        if let Ok(value) = api::get::<Config>("/config").await {
            config.set(Some(value));
        }
    });

    view! {
        <div class="system-info stack">
            {move || state.with(|s| s.pending_restart).then(|| view! {
                <Alert variant="warning" title="Restart required">
                    "Some changed settings apply only after slskr restarts."
                </Alert>
            })}
            <div class="grid grid--2 system-info__grid">
                <VersionCard state/>
                <ConnectionCard state config/>
                <DaemonCard config/>
                <NetworkCard state/>
            </div>
            <ApplicationCard/>
            <Card title="Diagnostics" description="The raw application state, as the API reports it.">
                <details class="system-raw">
                    <summary>"Show application state (JSON)"</summary>
                    <pre class="code code--scroll">
                        {move || serde_json::to_string_pretty(&state.get()).unwrap_or_else(|e| e.to_string())}
                    </pre>
                </details>
            </Card>
        </div>
    }
}

#[component]
fn VersionCard(state: RwSignal<State>) -> impl IntoView {
    let version = Memo::new(move |_| state.with(|s| s.version.clone()));
    view! {
        <Card title="Version" description="The running build.">
            <dl class="dl">
                <dt>"Current"</dt>
                <dd class="cluster">
                    <span class="mono">{move || version.with(|v| v.current.clone())}</span>
                    {move || version.with(|v| v.is_canary).then(|| view! { <Badge variant="purple" text="Canary"/> })}
                    {move || version.with(|v| v.is_development).then(|| view! { <Badge variant="outline" text="Development"/> })}
                </dd>
                {move || version.with(|v| (v.full != v.current).then(|| v.full.clone())).map(|full| view! {
                    <dt>"Build"</dt>
                    <dd class="mono text-sm">{full}</dd>
                })}
            </dl>
        </Card>
    }
}

#[component]
fn ConnectionCard(state: RwSignal<State>, config: RwSignal<Option<Config>>) -> impl IntoView {
    let server = Memo::new(move |_| state.with(|s| s.server.clone()));
    let connection = Memo::new(move |_| server.with(|s| s.state));
    let watchdog = Memo::new(move |_| state.with(|s| s.connection_watchdog.clone()));
    let listen = move || {
        config.with(|c| {
            c.as_ref().map_or_else(
                || "—".to_owned(),
                |c| format!("{}:{}", c.soulseek.listen_address, c.soulseek.listen_port),
            )
        })
    };
    view! {
        <Card title="Soulseek" description="The connection to the Soulseek server.">
            <dl class="dl">
                <dt>"Status"</dt>
                <dd class="cluster">
                    {move || {
                        let (variant, text) = connection_badge(connection.get());
                        view! {
                            <span class=format!("badge badge--{variant} system-connection")>
                                <StatusDot status=Signal::derive(move || connection_dot(connection.get()))/>
                                {text}
                            </span>
                        }
                    }}
                    {move || state.with(|s| s.pending_reconnect).then(|| view! {
                        <Badge variant="warning" text="Reconnect pending"
                            title="A changed Soulseek setting applies on the next reconnect"/>
                    })}
                </dd>
                <dt>"Username"</dt>
                <dd>{move || or_dash(state.with(|s| s.user.username.clone()))}</dd>
                <dt>"Server"</dt>
                <dd class="mono text-sm">{move || or_dash(server.with(|s| s.address.clone()))}</dd>
                {move || server.with(|s| s.endpoint.clone()).map(|endpoint| view! {
                    <dt>"Endpoint"</dt>
                    <dd class="mono text-sm">{endpoint}</dd>
                })}
                <dt>"Listening on"</dt>
                <dd class="mono text-sm">{listen}</dd>
                <dt>"Auto-reconnect"</dt>
                <dd>
                    {move || watchdog.with(|w| {
                        if !w.is_enabled {
                            "Off".to_owned()
                        } else if w.is_attempting_connection {
                            "Attempting to connect…".to_owned()
                        } else if let Some(next) = w.next_attempt_at {
                            format!("Next attempt {}", format::relative(next))
                        } else {
                            "On".to_owned()
                        }
                    })}
                </dd>
                <dt>"Privileges"</dt>
                <dd>
                    {move || state.with(|s| {
                        let privileges = &s.user.privileges;
                        if privileges.is_privileged {
                            let days = privileges.privileges_remaining / 86_400;
                            format!("Privileged, {days} days left")
                        } else {
                            "None".to_owned()
                        }
                    })}
                </dd>
                <dt>"Average upload speed"</dt>
                <dd class="tabular">
                    {move || state.with(|s| format_speed(f64::from(s.user.statistics.average_speed)))}
                </dd>
            </dl>
        </Card>
    }
}

#[component]
fn DaemonCard(config: RwSignal<Option<Config>>) -> impl IntoView {
    let row = move |label: &'static str, value: fn(&Config) -> String| {
        view! {
            <dt>{label}</dt>
            <dd class="mono text-sm">
                {move || config.with(|c| c.as_ref().map_or_else(|| "—".to_owned(), value))}
            </dd>
        }
    };
    view! {
        <Card title="Daemon" description="Where slskr listens and keeps its files.">
            <dl class="dl">
                {row("Web interface", |c| c.web.listen.to_string())}
                {row("URL base", |c| if c.web.url_base.is_empty() { "/".to_owned() } else { c.web.url_base.clone() })}
                {row("Downloads", |c| c.downloads.directory.clone())}
                {row("Incomplete", |c| c.downloads.incomplete_directory.clone())}
                {row("Log level", |c| c.logging.level.as_str().to_owned())}
                {row("Log files", |c| if c.logging.file { "On".to_owned() } else { "Off".to_owned() })}
            </dl>
        </Card>
    }
}

#[component]
fn NetworkCard(state: RwSignal<State>) -> impl IntoView {
    let network = Memo::new(move |_| state.with(|s| s.distributed_network.clone()));
    view! {
        <Card title="Distributed network" description="Search requests reach you through a parent and you pass them on to children.">
            <dl class="dl">
                <dt>"Parent"</dt>
                <dd>{move || network.with(|n| {
                    if n.is_branch_root {
                        "None (branch root)".to_owned()
                    } else {
                        or_dash(n.parent.clone().filter(|_| n.has_parent))
                    }
                })}</dd>
                <dt>"Branch"</dt>
                <dd>{move || network.with(|n| format!(
                    "Level {}{}",
                    n.branch_level,
                    n.branch_root.as_ref().map(|root| format!(", root {root}")).unwrap_or_default()
                ))}</dd>
                <dt>"Children"</dt>
                <dd class="cluster">
                    <span class="tabular">{move || network.with(|n| format!("{} of {}", n.children.len(), n.child_limit))}</span>
                    {move || if network.with(|n| n.can_accept_children) {
                        view! { <Badge variant="success" text="Accepting"/> }.into_any()
                    } else {
                        view! { <Badge text="Not accepting"/> }.into_any()
                    }}
                </dd>
            </dl>
        </Card>
    }
}

#[component]
fn ApplicationCard() -> impl IntoView {
    let message = RwSignal::new(None::<(&'static str, String)>);
    let busy = RwSignal::new(false);
    let run = move |path: &'static str, done: &'static str| {
        busy.set(true);
        message.set(None);
        leptos::task::spawn_local(async move {
            match api::post_empty::<serde_json::Value>(path).await {
                Ok(_) => message.try_set(Some(("info", done.to_owned()))),
                Err(err) => message.try_set(Some(("danger", err.to_string()))),
            };
            busy.try_set(false);
        });
    };
    view! {
        <Card title="Application"
            description="Restart to apply changed settings, or stop the daemon. Both end every transfer in progress; needs an administrator session.">
            <div class="cluster">
                <Modal size="sm" title="Restart slskr?"
                    trigger=move || view! {
                        <button type="button" class="btn system-restart" disabled=move || busy.get()>
                            <Icon name="rotate-cw"/>"Restart"
                        </button>
                    }
                    content="Transfers in progress stop and resume after the restart. This page reconnects automatically."
                    actions=vec![
                        ModalAction::new("Cancel"),
                        ModalAction::new("Restart").primary().on_click(move || {
                            run("/application/restart", "Restarting… this page reconnects when slskr is back.");
                        }),
                    ]/>
                <Modal size="sm" title="Shut down slskr?"
                    trigger=move || view! {
                        <button type="button" class="btn btn--danger-outline system-shutdown" disabled=move || busy.get()>
                            <Icon name="power"/>"Shut down"
                        </button>
                    }
                    content="The daemon stops and this page stops working until slskr is started again."
                    actions=vec![
                        ModalAction::new("Cancel"),
                        ModalAction::new("Shut down").negative().on_click(move || {
                            run("/application/shutdown", "Shutting down. Start slskr again to use this page.");
                        }),
                    ]/>
            </div>
            <div role="status" class="system-info__message">
                {move || message.get().map(|(variant, text)| view! {
                    <Alert variant=variant role="none">{text}</Alert>
                })}
            </div>
        </Card>
    }
}
