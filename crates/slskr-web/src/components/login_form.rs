use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use slskr_types::session::{LoginRequest, Session};

use super::app::AuthLayout;
use crate::{services::api, ui::Alert};

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
        <AuthLayout>
            <div class="card login-card">
                <div class="card__header">
                    <div>
                        <h1 class="card__title">"Sign in"</h1>
                        <p class="card__description">"Sign in to manage searches, transfers and shares"</p>
                    </div>
                </div>
                <form class="card__content stack" aria-busy=move || pending.get().to_string()
                    on:submit=move |event| {
                        event.prevent_default();
                        if ready.get_untracked() && !pending.get_untracked() {
                            error.set(None);
                            pending.set(true);
                        }
                    }>
                    <div class="field">
                        <label class="label" for="login-username">"Username"</label>
                        <input node_ref=username_input class="input" id="login-username" name="username"
                            autocomplete="username" placeholder="Enter username" required disabled=move || pending.get()
                            on:input=move |event| username.set(event_target_value(&event)) />
                    </div>
                    <div class="field">
                        <label class="label" for="login-password">"Password"</label>
                        <input class="input" type="password" id="login-password" name="password"
                            autocomplete="current-password" placeholder="Enter password" required
                            disabled=move || pending.get()
                            on:input=move |event| password.set(event_target_value(&event)) />
                    </div>
                    {move || error.get().map(|message| view! {
                        <Alert variant="danger" class="login-failure">{message}</Alert>
                    })}
                    <button type="submit" class="btn btn--primary btn--block login-button"
                        aria-busy=move || pending.get().then_some("true")
                        disabled=move || pending.get() || !ready.get()>
                        "Sign in"
                        {move || pending.get().then(|| view! { <span class="spinner" aria-hidden="true"></span> })}
                    </button>
                </form>
            </div>
            <p class="auth__note">
                <a href="https://github.com/slskd/slskd" rel="noreferrer" target="_blank">
                    "UI adapted from slskd (AGPLv3)"
                </a>
            </p>
        </AuthLayout>
    }
}
