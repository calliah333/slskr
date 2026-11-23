use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use slskr_types::session::{LoginRequest, Session};

use crate::services::api;

#[component]
pub(super) fn LoginForm(on_login: Callback<()>) -> impl IntoView {
    let username = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let pending = RwSignal::new(false);
    let error = RwSignal::new(None::<String>);
    let username_input = NodeRef::<leptos::html::Input>::new();
    let ready =
        Memo::new(move |_| !username.with(String::is_empty) && !password.with(String::is_empty));

    Effect::new(move |_| {
        if !pending.get() {
            if let Some(input) = username_input.get() {
                let _ = input.focus();
            }
            return;
        }
        let request = LoginRequest {
            username: username.get_untracked(),
            password: password.get_untracked(),
        };
        spawn_local_scoped_with_cancellation(async move {
            // The daemon sets an HttpOnly session cookie; the app then re-reads the session.
            match api::post::<_, Session>("/session", &request).await {
                Ok(_) => on_login.run(()),
                Err(err) => {
                    let message = if err.status == 401 {
                        "Invalid username or password.".into()
                    } else {
                        err.message
                    };
                    error.set(Some(message));
                    pending.set(false);
                }
            }
        });
    });

    view! {
        <div class="ui middle aligned center aligned grid" style="height: 100vh">
            <div class="column" style="max-width: 372px">
                <h1 class="ui center aligned header" style="font-family: monospace">"slskr"</h1>
                <form class="ui large form" aria-busy=move || pending.get().to_string()
                    on:submit=move |event| {
                        event.prevent_default();
                        if ready.get_untracked() && !pending.get_untracked() {
                            error.set(None);
                            pending.set(true);
                        }
                    }>
                    <div class="ui raised segment">
                        <div class="field">
                            <div class="ui left icon fluid input">
                                <input node_ref=username_input name="username" aria-label="Username"
                                    autocomplete="username" placeholder="Username" required disabled=move || pending.get()
                                    on:input=move |event| username.set(event_target_value(&event)) />
                                <i class="user icon" aria-hidden="true"></i>
                            </div>
                        </div>
                        <div class="field">
                            <div class="ui left icon fluid input">
                                <input type="password" name="password" aria-label="Password" autocomplete="current-password"
                                    placeholder="Password" required disabled=move || pending.get()
                                    on:input=move |event| password.set(event_target_value(&event)) />
                                <i class="lock icon" aria-hidden="true"></i>
                            </div>
                        </div>
                    </div>
                    <button type="submit" class="ui large fluid primary login-button button"
                        class:loading=move || pending.get() disabled=move || pending.get() || !ready.get()>
                        <i class="sign in icon" aria-hidden="true"></i>"Login"
                    </button>
                    {move || error.get().map(|message| view! {
                        <div class="ui floating negative message login-failure" role="alert">
                            <i class="x icon" aria-hidden="true"></i>{message}
                        </div>
                    })}
                </form>
                <p>
                    <a href="https://github.com/slskd/slskd" rel="noreferrer" target="_blank">
                        "UI adapted from slskd (AGPLv3)"
                    </a>
                </p>
            </div>
        </div>
    }
}
