use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use slskr_api::session::{LoginRequest, TokenResponse};

use crate::services::{
    api,
    token::{self, TokenStorage},
};

#[component]
pub(super) fn LoginForm(on_login: Callback<()>) -> impl IntoView {
    let username = RwSignal::new(String::new());
    let password = RwSignal::new(String::new());
    let remember = RwSignal::new(true);
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
            username: Some(username.get_untracked()),
            password: Some(password.get_untracked()),
        };
        let storage = TokenStorage::for_remember_me(remember.get_untracked());
        spawn_local_scoped_with_cancellation(async move {
            match api::post::<_, TokenResponse>("/session", &request).await {
                Ok(response) if !response.token.is_empty() => {
                    token::clear_token();
                    token::set_token(storage, &response.token);
                    on_login.run(());
                }
                Ok(_) => {
                    error.set(Some("The server returned an empty session token.".into()));
                    pending.set(false);
                }
                Err(err) => {
                    let message = if err.status == 401 {
                        "Invalid username or password.".into()
                    } else {
                        serde_json::from_str::<String>(&err.message).unwrap_or(err.message)
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
                        <div class="ui checked checkbox" class:checked=move || remember.get()>
                            <input id="login-remember" type="checkbox" name="remember" prop:checked=move || remember.get()
                                disabled=move || pending.get() on:change=move |event| remember.set(event_target_checked(&event)) />
                            <label for="login-remember">"Remember Me"</label>
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
