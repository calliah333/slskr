//! Account: the Soulseek account (with connection status and a login test), the profile
//! peers see, and the web sign-in.

use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use slskr_types::{server::ConnectionState, setup::Credentials};

use super::fields::{Kind, Setting, use_form};
use crate::{
    components::{app::AppContext, setup::test_login},
    services::api,
    ui::{
        Badge, Icon,
        form::{Advanced, Section},
    },
};

/// The live connection: a status badge and a Connect / Disconnect button.
#[component]
fn Connection() -> impl IntoView {
    let Some(AppContext { state, .. }) = use_context::<AppContext>() else {
        return ().into_any();
    };
    let pending = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let connected = Memo::new(move |_| state.with(|state| state.server.state.is_connected()));
    // (badge variant, text, busy)
    let status = Memo::new(move |_| {
        state.with(|state| {
            if state.server.state.is_connected() {
                ("success", "Connected", false)
            } else if matches!(
                state.server.state,
                ConnectionState::Connecting | ConnectionState::LoggingIn
            ) {
                ("warning", "Connecting", true)
            } else if state.connection_watchdog.is_attempting_connection {
                ("warning", "Reconnecting", false)
            } else {
                ("neutral", "Disconnected", false)
            }
        })
    });
    let toggle = move |_| {
        let connected = connected.get_untracked();
        pending.set(true);
        error.set(None);
        spawn_local_scoped_with_cancellation(async move {
            let result = if connected {
                api::post::<_, serde_json::Value>(
                    "/server/disconnect",
                    &serde_json::json!({"message": "client disconnected from web UI"}),
                )
                .await
                .map(|_| ())
            } else {
                api::post_empty::<serde_json::Value>("/server/connect")
                    .await
                    .map(|_| ())
            };
            if let Err(failure) = result {
                error.set(Some(failure.message));
            }
            pending.set(false);
        });
    };
    view! {
        <div class="settings-connection">
            {move || {
                let (variant, text, busy) = status.get();
                view! {
                    <span class=format!("badge badge--{variant} settings-connection-status") role="status">
                        {if busy {
                            view! { <span class="spinner spinner--xs" aria-hidden="true"></span> }.into_any()
                        } else {
                            view! { <span class="settings-connection-dot" aria-hidden="true"></span> }.into_any()
                        }}
                        {text}
                    </span>
                }
            }}
            <button type="button" class="btn btn--sm settings-connect" disabled=pending
                aria-busy=move || pending.get().then_some("true") on:click=toggle>
                {move || if pending.get() {
                    view! { <span class="spinner" aria-hidden="true"></span> }.into_any()
                } else if connected.get() {
                    view! { <Icon name="unplug"/> }.into_any()
                } else {
                    view! { <Icon name="plug"/> }.into_any()
                }}
                {move || if connected.get() { "Disconnect" } else { "Connect" }}
            </button>
        </div>
        {move || error.get().map(|message| view! {
            <p class="error settings-connection-error" role="alert">{message}</p>
        })}
    }
    .into_any()
}

#[component]
pub fn SoulseekAccount() -> impl IntoView {
    let form = use_form();
    let testing = RwSignal::new(false);
    let result = RwSignal::new(None::<(bool, String)>);
    // Clear an old result when the username or password changes.
    Effect::new(move |_| {
        form.value("soulseek.username");
        form.value("soulseek.password");
        result.set(None);
    });
    let test = move |_| {
        let username = super::model::text_of(&form.value("soulseek.username"));
        let password = super::model::text_of(&form.value("soulseek.password"));
        if username.trim().is_empty() || password.is_empty() {
            result.set(Some((
                false,
                "Type the password under Change first: slskr never shows the saved one.".to_owned(),
            )));
            return;
        }
        testing.set(true);
        result.set(None);
        spawn_local_scoped_with_cancellation(async move {
            let credentials = Credentials { username, password };
            let answer = test_login(credentials.clone()).await;
            if testing.try_set(false).is_none()
                && super::model::text_of(&form.value("soulseek.username")) == credentials.username
                && super::model::text_of(&form.value("soulseek.password")) == credentials.password
            {
                result.set(Some(answer));
            }
        });
    };
    view! {
        <Section id="settings-soulseek" title="Soulseek account"
            actions=ViewFn::from(|| view! { <Connection/> })
            footer=ViewFn::from(move || view! {
                <button type="button" class="btn btn--sm settings-test-login" disabled=testing
                    aria-busy=move || testing.get().then_some("true") on:click=test>
                    {move || if testing.get() {
                        view! { <span class="spinner" aria-hidden="true"></span> }.into_any()
                    } else {
                        view! { <Icon name="log-in"/> }.into_any()
                    }}
                    "Test login"
                </button>
                {move || match result.get() {
                    Some((ok, message)) => view! {
                        <span class=if ok { "settings-test-result text-success" } else { "settings-test-result text-danger" }
                            role=if ok { "status" } else { "alert" }>
                            <Icon name=if ok { "circle-check" } else { "circle-x" }/>{message}
                        </span>
                    }.into_any(),
                    None => ().into_any(),
                }}
            })>
            <Setting key="soulseek.username" label="Username" kind=Kind::Text
                help="If the username isn't registered yet, logging in creates it."/>
            <Setting key="soulseek.password" label="Password" kind=Kind::Secret { confirm: false }/>
            <Advanced open=Signal::derive(move || form.error("soulseek.server").is_some() || form.error("soulseek.connect").is_some())>
                <Setting key="soulseek.server" label="Server" kind=Kind::Mono/>
                <Setting key="soulseek.connect" label="Connect on start" kind=Kind::Toggle restart=true/>
            </Advanced>
        </Section>
    }
}

#[component]
pub fn Profile() -> impl IntoView {
    view! {
        <Section id="settings-profile" title="Profile">
            <Setting key="soulseek.description" label="Description" kind=Kind::TextArea/>
            <Setting key="soulseek.picture" label="Picture" kind=Kind::Mono
                placeholder="/path/to/picture.png"
                help="A path on the slskr machine."/>
            <Setting key="soulseek.rooms" label="Rooms to join" kind=Kind::List("Room name, then Enter")/>
        </Section>
    }
}

#[component]
pub fn WebLogin() -> impl IntoView {
    let form = use_form();
    view! {
        <Section id="settings-web" title="Web sign-in">
            <Setting key="web.username" label="Username" kind=Kind::Text/>
            <Setting key="web.password" label="Password" kind=Kind::Secret { confirm: true }
                help="At least 8 characters."/>
            <Show when=move || form.changes_sign_in()>
                <div class="alert alert--warning settings-signout-warning" role="note">
                    <Icon name="log-out"/>
                    <div class="alert__body">
                        <p>"Saving signs out every browser, including this one. Sign in again with the new credentials."</p>
                    </div>
                </div>
            </Show>
            <Setting key="web.session_ttl" label="Stay signed in for" kind=Kind::Duration
                help="Each use extends it; 5 minutes to 365 days."/>
            <Advanced open=Signal::derive(move || form.error("web.auth").is_some())>
                <Setting key="web.auth" label="Require sign-in" kind=Kind::Toggle restart=true
                    help="Off disables authentication: only for loopback or development."/>
            </Advanced>
        </Section>
    }
}

/// A small "from slskr.toml" badge.
#[component]
pub fn ConfigBadge(#[prop(into)] title: String) -> impl IntoView {
    view! { <Badge variant="outline" icon="lock" title=title text="slskr.toml"/> }
}
