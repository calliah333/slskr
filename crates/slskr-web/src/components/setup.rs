//! First-run setup wizard at `/setup`: setup code (remote callers), web account, Soulseek
//! account, shares, review. Finishing posts `/api/setup`, which signs the browser in.

use std::collections::BTreeMap;

use leptos::{prelude::*, task::spawn_local_scoped_with_cancellation};
use slskr_types::setup::{
    Credentials, DirectoryListing, LoginTestFailure, LoginTestResult, SetupRequest, SetupStatus,
};

use crate::{
    components::directory_picker::DirectoryPicker,
    services::{api, forms},
    ui::form::{Field, FormMessage, ListEditor, PasswordInput, TextInput, focus_first_invalid},
};

/// Minimum web password length (`POST /api/setup` refuses shorter ones).
const MIN_PASSWORD_LENGTH: usize = 8;
/// Longest Soulseek username the server accepts.
const SOULSEEK_USERNAME_LIMIT: usize = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Step {
    Welcome,
    Web,
    Soulseek,
    Shares,
    Review,
}

impl Step {
    const ALL: [Step; 5] = [
        Step::Welcome,
        Step::Web,
        Step::Soulseek,
        Step::Shares,
        Step::Review,
    ];

    fn title(self) -> &'static str {
        match self {
            Step::Welcome => "Welcome",
            Step::Web => "Web account",
            Step::Soulseek => "Soulseek",
            Step::Shares => "Shares",
            Step::Review => "Review",
        }
    }

    fn index(self) -> usize {
        Step::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    fn next(self) -> Step {
        Step::ALL[(self.index() + 1).min(Step::ALL.len() - 1)]
    }

    fn previous(self) -> Step {
        Step::ALL[self.index().saturating_sub(1)]
    }

    /// The step showing the field an error is about.
    fn of_error(key: &str) -> Option<Step> {
        if key == "code" {
            Some(Step::Welcome)
        } else if key.starts_with("web.") {
            Some(Step::Web)
        } else if key.starts_with("soulseek.") {
            Some(Step::Soulseek)
        } else if key == "shares" || key.starts_with("shares.") {
            Some(Step::Shares)
        } else {
            None
        }
    }
}

/// A friendly sentence for a failed Soulseek login test.
fn login_test_message(result: &LoginTestResult) -> String {
    let detail = result.message.clone().unwrap_or_default();
    match result.reason {
        None => "Logged in to Soulseek.".to_owned(),
        Some(LoginTestFailure::InvalidPassword) => {
            "Wrong password for this Soulseek username.".to_owned()
        }
        Some(LoginTestFailure::InvalidUsername) => {
            "The Soulseek server does not accept this username; try another one.".to_owned()
        }
        Some(LoginTestFailure::ServerFull) => {
            "The Soulseek server is full right now; try again later.".to_owned()
        }
        Some(LoginTestFailure::ServerPrivate) => {
            "The Soulseek server only admits invited users.".to_owned()
        }
        Some(LoginTestFailure::InvalidVersion) => {
            "The Soulseek server refused this client version.".to_owned()
        }
        Some(LoginTestFailure::Rejected) => {
            format!("The Soulseek server refused the login: {detail}")
        }
        Some(LoginTestFailure::Unreachable) => {
            format!("Could not reach the Soulseek server ({detail}). Check the network or proxy.")
        }
        Some(LoginTestFailure::Timeout) => "The Soulseek server did not answer in time.".to_owned(),
        Some(LoginTestFailure::Error) => format!("The login test failed: {detail}"),
    }
}

fn soulseek_username_problem(username: &str) -> Option<String> {
    if username.trim().is_empty() {
        Some("Enter your Soulseek username".to_owned())
    } else if username.trim() != username {
        Some("The username can not start or end with a space".to_owned())
    } else if username.chars().count() > SOULSEEK_USERNAME_LIMIT {
        Some(format!(
            "Soulseek usernames have at most {SOULSEEK_USERNAME_LIMIT} characters"
        ))
    } else {
        None
    }
}

fn rate_limited(error: &api::ApiError) -> String {
    match error.retry_after {
        Some(seconds) => format!(
            "Too many wrong setup codes. Try again in {seconds} second{}.",
            if seconds == 1 { "" } else { "s" }
        ),
        None => error.message.clone(),
    }
}

/// The wizard. `on_done` runs once setup is complete and the browser is signed in (or when
/// someone else completed setup meanwhile).
#[component]
pub fn SetupWizard(on_done: Callback<()>) -> impl IntoView {
    let status = RwSignal::new(None::<SetupStatus>);
    let load_error = RwSignal::new(None::<String>);
    spawn_local_scoped_with_cancellation(async move {
        match api::get::<SetupStatus>("/setup").await {
            Ok(loaded) if !loaded.required => on_done.run(()),
            Ok(loaded) => status.set(Some(loaded)),
            Err(error) => load_error.set(Some(error.message)),
        }
    });
    view! {
        <main class="setup">
            <h1 class="ui header setup-title">"slskr setup"</h1>
            {move || load_error.get().map(|message| view! {
                <FormMessage kind="negative"><p>{"Could not load the setup status: "}{message}</p></FormMessage>
            })}
            {move || match status.get() {
                Some(status) => view! { <Wizard status on_done/> }.into_any(),
                None if load_error.with(Option::is_none) => view! {
                    <div class="ui active centered inline loader" role="status" aria-label="Loading"></div>
                }.into_any(),
                None => ().into_any(),
            }}
        </main>
    }
}

#[component]
fn Wizard(status: SetupStatus, on_done: Callback<()>) -> impl IntoView {
    let step = RwSignal::new(Step::Welcome);
    let busy = RwSignal::new(false);
    let errors = RwSignal::new(BTreeMap::<String, String>::new());
    let top_error = RwSignal::new(None::<String>);

    let code = RwSignal::new(String::new());
    let web_username = RwSignal::new("admin".to_owned());
    let web_password = RwSignal::new(String::new());
    let web_confirm = RwSignal::new(String::new());
    let soulseek_username = RwSignal::new(String::new());
    let soulseek_password = RwSignal::new(String::new());
    let test_result = RwSignal::new(None::<(bool, String)>);
    let testing = RwSignal::new(false);
    let shares = RwSignal::new(Vec::<String>::new());
    let picking = RwSignal::new(false);
    let heading = NodeRef::<leptos::html::H2>::new();

    let error_of =
        move |key: &'static str| Signal::derive(move || errors.with(|e| e.get(key).cloned()));
    let set_errors = move |found: Vec<(&'static str, String)>| {
        errors.set(found.into_iter().map(|(k, v)| (k.to_owned(), v)).collect());
    };
    let clear_error = move |key: &'static str| {
        errors.update(|e| {
            e.remove(key);
        });
    };

    // Focus the first control of each step when the step changes: an invalid one, else the
    // first input, else the Next button (so Enter advances), else the heading.
    Effect::new(move |_| {
        step.track();
        request_animation_frame(move || {
            let find = |selector: &str| {
                document()
                    .query_selector(selector)
                    .ok()
                    .flatten()
                    .and_then(|e| wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(e).ok())
            };
            let first = find(".setup-step [aria-invalid=true]")
                .or_else(|| find(".setup-step input:not([disabled])"))
                .or_else(|| find(".setup-step button[type=submit]"));
            match first {
                Some(element) => {
                    let _ = element.focus();
                }
                None => {
                    if let Some(heading) = heading.get_untracked() {
                        let _ = heading.focus();
                    }
                }
            }
        });
    });

    let soulseek_given = move || {
        !soulseek_username.with(String::is_empty) || !soulseek_password.with(String::is_empty)
    };

    // Checks the current step; true when it may be left forward.
    let validate = move |current: Step| -> bool {
        let mut found = Vec::new();
        match current {
            Step::Welcome if status.code_required && code.with(|c| c.trim().is_empty()) => {
                found.push((
                    "code",
                    "Enter the setup code from the daemon log".to_owned(),
                ));
            }
            Step::Web => {
                if !status.web_username_pinned && web_username.with(|u| u.trim().is_empty()) {
                    found.push(("web.username", "Choose a username".to_owned()));
                }
                if !status.web_password_pinned {
                    let password = web_password.get_untracked();
                    if password.chars().count() < MIN_PASSWORD_LENGTH {
                        found.push((
                            "web.password",
                            format!("Use at least {MIN_PASSWORD_LENGTH} characters"),
                        ));
                    } else if password != web_confirm.get_untracked() {
                        found.push(("web.confirm", "The passwords do not match".to_owned()));
                    }
                }
            }
            Step::Soulseek if !status.soulseek_pinned && soulseek_given() => {
                if let Some(problem) = soulseek_username_problem(&soulseek_username.get_untracked())
                {
                    found.push(("soulseek.username", problem));
                }
                if soulseek_password.with(String::is_empty) {
                    found.push((
                        "soulseek.password",
                        "Enter your Soulseek password".to_owned(),
                    ));
                }
            }
            _ => {}
        }
        let ok = found.is_empty();
        set_errors(found);
        if !ok {
            focus_first_invalid(".setup-step");
        }
        ok
    };

    let finish = move || {
        let request = SetupRequest {
            code: status
                .code_required
                .then(|| code.get_untracked().trim().to_owned()),
            web: Credentials {
                username: if status.web_username_pinned {
                    String::new()
                } else {
                    web_username.get_untracked().trim().to_owned()
                },
                password: web_password.get_untracked(),
            },
            soulseek: (!status.soulseek_pinned && soulseek_given()).then(|| Credentials {
                username: soulseek_username.get_untracked(),
                password: soulseek_password.get_untracked(),
            }),
            shares: Some(shares.get_untracked()).filter(|s| !s.is_empty()),
        };
        busy.set(true);
        top_error.set(None);
        spawn_local_scoped_with_cancellation(async move {
            match api::post::<_, slskr_types::session::Session>("/setup", &request).await {
                Ok(_) => {
                    api::set_setup_code(None);
                    on_done.run(());
                    return;
                }
                Err(error) if error.status == 403 => {
                    set_errors(vec![(
                        "code",
                        "That setup code is not right. Use the code from the current daemon log."
                            .to_owned(),
                    )]);
                    step.set(Step::Welcome);
                }
                Err(error) if error.status == 429 => top_error.set(Some(rate_limited(&error))),
                Err(error) if error.status == 409 && error.message.contains("already complete") => {
                    on_done.run(());
                    return;
                }
                Err(error) if matches!(error.status, 400 | 409) => {
                    let mut found = BTreeMap::new();
                    let mut rest = Vec::new();
                    for (key, text) in forms::error_parts(&error.message) {
                        let field = key.as_deref().and_then(|key| match key {
                            "web.username" | "web.password" | "soulseek.username"
                            | "soulseek.password" => Some(key.to_owned()),
                            key if key == "shares" || key.starts_with("shares.") => {
                                Some("shares".to_owned())
                            }
                            _ => None,
                        });
                        match field {
                            Some(field) => {
                                found
                                    .entry(field)
                                    .and_modify(|t: &mut String| {
                                        t.push_str("; ");
                                        t.push_str(&text);
                                    })
                                    .or_insert(text);
                            }
                            None => rest.push(text),
                        }
                    }
                    let first = found.keys().filter_map(|key| Step::of_error(key)).min();
                    errors.set(found);
                    top_error.set((!rest.is_empty()).then(|| rest.join("; ")));
                    if let Some(first) = first {
                        step.set(first);
                    }
                }
                Err(error) => top_error.set(Some(error.message)),
            }
            busy.set(false);
        });
    };

    let advance = move || {
        if busy.get_untracked() {
            return;
        }
        let current = step.get_untracked();
        if !validate(current) {
            return;
        }
        match current {
            Step::Welcome if status.code_required => {
                // Check the code now, with a setup helper route.
                api::set_setup_code(Some(code.get_untracked().trim().to_owned()));
                busy.set(true);
                spawn_local_scoped_with_cancellation(async move {
                    match api::get::<DirectoryListing>("/fs/dirs").await {
                        Ok(_) => step.set(Step::Web),
                        // Any other answer (an unreadable home, say) still accepted the code.
                        Err(error) if error.status == 403 => set_errors(vec![(
                            "code",
                            "That setup code is not right. Use the code from the current daemon log."
                                .to_owned(),
                        )]),
                        Err(error) if error.status == 429 => {
                            set_errors(vec![("code", rate_limited(&error))]);
                        }
                        Err(_) => step.set(Step::Web),
                    }
                    busy.set(false);
                });
            }
            Step::Review => finish(),
            _ => step.set(current.next()),
        }
    };

    let test_login = move |_| {
        if !validate(Step::Soulseek) || !soulseek_given() {
            if !soulseek_given() {
                set_errors(vec![(
                    "soulseek.username",
                    "Enter a username and password to test".to_owned(),
                )]);
            }
            return;
        }
        let credentials = Credentials {
            username: soulseek_username.get_untracked(),
            password: soulseek_password.get_untracked(),
        };
        testing.set(true);
        test_result.set(None);
        spawn_local_scoped_with_cancellation(async move {
            let result =
                api::post::<_, LoginTestResult>("/soulseek/test-login", &credentials).await;
            test_result.set(Some(match result {
                Ok(result) => (result.ok, login_test_message(&result)),
                Err(error) if error.status == 429 => (
                    false,
                    "Another login test is running; try again in a moment.".to_owned(),
                ),
                Err(error) if error.status == 409 => (
                    false,
                    format!("Can not test the login now: {}", error.message),
                ),
                Err(error) => (false, error.message),
            }));
            testing.set(false);
        });
    };

    let step_view = move || {
        let current = step.get();
        let body = match current {
            Step::Welcome => view! {
                <p>"slskr is almost ready. Set up a web sign-in, optionally connect your Soulseek account and choose folders to share. Everything can be changed later in System → Settings."</p>
                {status.code_required.then(|| view! {
                    <Field id="setup-code" label="Setup code" error=error_of("code")
                        help="This browser is not on the slskr machine, so prove you can read its log: the code is printed in the daemon log (with Docker: docker compose logs slskr).">
                        <TextInput value=code on_input=move |t: String| { code.set(t); clear_error("code"); }
                            placeholder="XXXX-XXXX-XXXX" autocomplete="one-time-code" name="code"/>
                    </Field>
                })}
            }.into_any(),
            Step::Web => {
                let username_lock = Signal::derive(move || status.web_username_pinned
                    .then(|| "Set in slskr.toml or environment (web.username)".to_owned()));
                let password_lock = Signal::derive(move || status.web_password_pinned
                    .then(|| "Set in slskr.toml or environment (web.password)".to_owned()));
                view! {
                    <p>"You sign in to this web UI with these."</p>
                    <Field id="setup-web-username" label="Username" error=error_of("web.username") locked_reason=username_lock>
                        <TextInput value=Signal::derive(move || if status.web_username_pinned { String::new() } else { web_username.get() })
                            placeholder=if status.web_username_pinned { "(configured)" } else { "" }
                            on_input=move |t: String| { web_username.set(t); clear_error("web.username"); }
                            autocomplete="username" name="username"/>
                    </Field>
                    <Field id="setup-web-password" label="Password" error=error_of("web.password") locked_reason=password_lock
                        help="At least 8 characters.">
                        <PasswordInput value=web_password name="password"
                            on_input=move |t: String| { web_password.set(t); clear_error("web.password"); }/>
                    </Field>
                    <Field id="setup-web-confirm" label="Repeat the password" error=error_of("web.confirm") locked_reason=password_lock>
                        <PasswordInput value=web_confirm name="confirm"
                            on_input=move |t: String| { web_confirm.set(t); clear_error("web.confirm"); }/>
                    </Field>
                }.into_any()
            }
            Step::Soulseek if status.soulseek_pinned => view! {
                <FormMessage kind="info">
                    <p>"The Soulseek account is set in slskr.toml or the environment (soulseek.username, soulseek.password)."</p>
                </FormMessage>
            }.into_any(),
            Step::Soulseek => view! {
                <p>"Optional: the account slskr uses on the Soulseek network. You can skip this and connect later in Settings."</p>
                <Field id="setup-soulseek-username" label="Soulseek username" error=error_of("soulseek.username")
                    help="If the username isn't registered yet, logging in creates it.">
                    <TextInput value=soulseek_username name="soulseek-username" autocomplete="off"
                        on_input=move |t: String| { soulseek_username.set(t); clear_error("soulseek.username"); test_result.set(None); }/>
                </Field>
                <Field id="setup-soulseek-password" label="Soulseek password" error=error_of("soulseek.password")>
                    <PasswordInput value=soulseek_password name="soulseek-password" autocomplete="off"
                        on_input=move |t: String| { soulseek_password.set(t); clear_error("soulseek.password"); test_result.set(None); }/>
                </Field>
                <div class="form-actions">
                    <button type="button" class="ui button setup-test-login" class:loading=testing disabled=testing
                        on:click=test_login>
                        "Test login"
                    </button>
                    {move || test_result.get().map(|(ok, message)| view! {
                        <span class=if ok { "ui green basic label" } else { "ui red basic label" }
                            role=if ok { "status" } else { "alert" }>{message}</span>
                    })}
                </div>
            }.into_any(),
            Step::Shares => view! {
                <p>"Optional: folders other users can browse and download from. You can add more later."</p>
                <Field id="setup-shares" label="Shared folders" error=error_of("shares")
                    help="Absolute paths on the slskr machine. Choose with Browse or type a path.">
                    <ListEditor items=shares placeholder="/path/to/music"
                        on_change=Callback::new(move |items: Vec<String>| { shares.set(items); clear_error("shares"); })
                        extra=ViewFn::from(move || view! {
                            <button type="button" class="ui button directory-browse" on:click=move |_| picking.set(true)>
                                <i class="folder open icon" aria-hidden="true"></i>"Browse…"
                            </button>
                        })/>
                </Field>
            }.into_any(),
            Step::Review => view! {
                <dl class="setup-review">
                    <dt>"Web sign-in"</dt>
                    <dd>{if status.web_username_pinned { "the configured username".to_owned() } else { web_username.get_untracked().trim().to_owned() }}</dd>
                    <dt>"Soulseek"</dt>
                    <dd>{if status.soulseek_pinned {
                        "set in slskr.toml or the environment".to_owned()
                    } else if soulseek_given() {
                        soulseek_username.get_untracked()
                    } else {
                        "not set up: connect later in System → Settings".to_owned()
                    }}</dd>
                    <dt>"Shares"</dt>
                    <dd>{move || {
                        let list = shares.get();
                        if list.is_empty() { "none".to_owned() } else { list.join(", ") }
                    }}</dd>
                </dl>
            }.into_any(),
        };
        let optional = matches!(current, Step::Soulseek | Step::Shares)
            && !(current == Step::Soulseek && status.soulseek_pinned);
        let skip = move |_| {
            if current == Step::Soulseek {
                soulseek_username.set(String::new());
                soulseek_password.set(String::new());
                test_result.set(None);
            } else {
                shares.set(Vec::new());
            }
            errors.set(BTreeMap::new());
            step.set(current.next());
        };
        view! {
            <form class="ui form segment setup-step" novalidate
                aria-labelledby="setup-step-title"
                on:submit=move |event| { event.prevent_default(); advance(); }>
                <h2 class="ui header" id="setup-step-title" tabindex="-1" node_ref=heading>{current.title()}</h2>
                {body}
                <div class="setup-actions">
                    {(current != Step::Welcome).then(|| view! {
                        <button type="button" class="ui basic button setup-back" disabled=busy
                            on:click=move |_| { errors.set(BTreeMap::new()); step.set(current.previous()); }>
                            "Back"
                        </button>
                    })}
                    {optional.then(|| view! {
                        <button type="button" class="ui button setup-skip" disabled=busy on:click=skip>"Skip"</button>
                    })}
                    <button type="submit" class="ui primary button setup-next" class:loading=busy disabled=busy>
                        {if current == Step::Review { "Finish" } else { "Next" }}
                    </button>
                </div>
            </form>
        }
    };

    view! {
        <ol class="ui mini ordered steps setup-steps" aria-label="Setup progress">
            {Step::ALL.into_iter().map(|s| {
                let state = move || {
                    let current = step.get();
                    if s == current { "active step" } else if s < current { "completed step" } else { "disabled step" }
                };
                view! {
                    <li class=state aria-current=move || (step.get() == s).then_some("step")>
                        <div class="content"><div class="title">{s.title()}</div></div>
                    </li>
                }
            }).collect_view()}
        </ol>
        <p class="visually-hidden" aria-live="polite">
            {move || format!("Step {} of {}: {}", step.get().index() + 1, Step::ALL.len(), step.get().title())}
        </p>
        {move || top_error.get().map(|message| view! { <FormMessage kind="negative"><p>{message}</p></FormMessage> })}
        {step_view}
        <DirectoryPicker open=picking on_pick=Callback::new(move |path: String| {
            shares.update(|list| if !list.contains(&path) { list.push(path) });
            clear_error("shares");
        })/>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_and_messages() {
        assert_eq!(Step::Welcome.next(), Step::Web);
        assert_eq!(Step::Review.next(), Step::Review);
        assert_eq!(Step::Welcome.previous(), Step::Welcome);
        assert_eq!(Step::of_error("shares"), Some(Step::Shares));
        assert_eq!(Step::of_error("web.password"), Some(Step::Web));
        assert_eq!(Step::of_error("other"), None);
        let failed = LoginTestResult {
            ok: false,
            reason: Some(LoginTestFailure::InvalidPassword),
            message: Some("INVALIDPASS".into()),
        };
        assert_eq!(
            login_test_message(&failed),
            "Wrong password for this Soulseek username."
        );
        assert!(soulseek_username_problem(" x").is_some());
        assert!(soulseek_username_problem(&"x".repeat(31)).is_some());
        assert!(soulseek_username_problem("someone").is_none());
    }
}
