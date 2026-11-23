//! A settings field bound to a dotted key of the [`Form`].

use leptos::prelude::*;
use serde_json::Value;

use super::model::{Form, Parse, field_id};
use crate::ui::form::{
    DurationInput, Field, ListEditor, NumberInput, PasswordInput, Select, TextArea, TextInput,
    Toggle,
};

/// The control of a [`Setting`].
#[derive(Clone)]
pub enum Kind {
    Text,
    TextArea,
    Number {
        min: u64,
        max: u64,
    },
    Duration,
    /// A duration where empty means never.
    OptionalDuration,
    Toggle,
    Select(Vec<(&'static str, &'static str)>),
    /// A list of strings; the text is the add input's placeholder.
    List(&'static str),
    /// A list of local directories, with a folder picker.
    Directories,
    /// A write-only value; `confirm` asks for it twice.
    Secret {
        confirm: bool,
    },
}

impl Kind {
    pub const COUNT: Self = Self::Number {
        min: 0,
        max: u32::MAX as u64,
    };
}

pub fn use_form() -> Form {
    expect_context()
}

/// Minimum length of a web password set here (as in first-run setup).
pub const MIN_PASSWORD_LENGTH: usize = 8;

/// One setting: label, control, help, the lock when pinned, errors and reset.
#[component]
pub fn Setting(
    #[prop(into)] key: String,
    #[prop(into)] label: String,
    kind: Kind,
    #[prop(into, optional)] help: Option<String>,
    /// Takes effect after a restart.
    #[prop(optional)]
    restart: bool,
    #[prop(into, optional)] placeholder: Option<String>,
) -> impl IntoView {
    let form = use_form();
    form.register(&key);
    let key = StoredValue::new(key);
    let k = move || key.get_value();
    let locked = Signal::derive(move || {
        form.pinned_by(&k())
            .map(|pinned| format!("Set in slskr.toml or environment ({pinned})"))
    });
    let error = Signal::derive(move || form.error(&k()));
    let editing_secret = RwSignal::new(false);
    let actions = ViewFn::from(move || {
        let reset = move |_| {
            let key = k();
            leptos::task::spawn_local(async move {
                let path = format!(
                    "/settings/{}",
                    crate::services::util::encode_uri_component(&key)
                );
                match crate::services::api::delete::<slskr_types::settings::Settings>(&path).await {
                    Ok(settings) => {
                        form.revert(&key);
                        form.load(settings);
                    }
                    Err(error) => {
                        form.top_error.try_set(Some(error.message));
                    }
                }
            });
        };
        view! {
            <Show when=move || form.is_dirty(&k())>
                <button type="button" class="ui mini basic button form-revert"
                    on:click=move |_| { editing_secret.set(false); form.revert(&k()); }>
                    "Undo change"
                </button>
            </Show>
            <Show when=move || !form.is_dirty(&k()) && form.is_stored(&k()) && form.pinned_by(&k()).is_none()>
                <button type="button" class="ui mini basic button form-reset" on:click=reset
                    title="Remove the value saved from this page">
                    "Reset to default"
                </button>
            </Show>
        }
    });
    let tag = restart.then_some("restart");
    let text = Signal::derive(move || form.text(&k()));
    // Built inside the Field, so the control finds its FieldContext.
    let control = move || match kind {
        Kind::Text => view! {
            <TextInput value=text placeholder=placeholder.clone().unwrap_or_default()
                on_input=move |t: String| form.input(&k(), Parse::Text, t)/>
        }
        .into_any(),
        Kind::TextArea => view! {
            <TextArea value=text placeholder=placeholder.clone().unwrap_or_default()
                on_input=move |t: String| form.input(&k(), Parse::Text, t)/>
        }
        .into_any(),
        Kind::Number { min, max } => view! {
            <NumberInput value=text placeholder=placeholder.clone().unwrap_or_default()
                on_input=move |t: String| form.input(&k(), Parse::Number { min, max }, t)/>
        }
        .into_any(),
        Kind::Duration => view! {
            <DurationInput value=Signal::derive(move || form.duration_text(&k()))
                on_input=move |t: String| form.input(&k(), Parse::Duration { optional: false }, t)/>
        }
        .into_any(),
        Kind::OptionalDuration => view! {
            <DurationInput value=Signal::derive(move || form.duration_text(&k())) optional=true
                on_input=move |t: String| form.input(&k(), Parse::Duration { optional: true }, t)/>
        }
        .into_any(),
        Kind::Toggle => view! {
            <Toggle checked=Signal::derive(move || form.value(&k()).as_bool().unwrap_or(false))
                on_change=move |on: bool| form.set(&k(), Value::Bool(on))/>
        }
        .into_any(),
        Kind::Select(options) => view! {
            <Select value=text
                options=options.into_iter().map(|(v, l)| (v.to_owned(), l.to_owned())).collect()
                on_change=move |v: String| form.set(&k(), Value::String(v))/>
        }
        .into_any(),
        Kind::List(placeholder) => {
            let items = Signal::derive(move || {
                form.value(&k())
                    .as_array()
                    .map(|items| items.iter().map(super::model::text_of).collect())
                    .unwrap_or_default()
            });
            view! {
                <ListEditor items placeholder=placeholder
                    on_change=Callback::new(move |items: Vec<String>| {
                        form.set(&k(), Value::Array(items.into_iter().map(Value::String).collect()))
                    })/>
            }
            .into_any()
        }
        Kind::Directories => {
            let items = Signal::derive(move || {
                form.value(&k())
                    .as_array()
                    .map(|items| items.iter().map(super::model::text_of).collect::<Vec<_>>())
                    .unwrap_or_default()
            });
            let set = move |items: Vec<String>| {
                form.set(
                    &k(),
                    Value::Array(items.into_iter().map(Value::String).collect()),
                );
            };
            let picking = RwSignal::new(false);
            let locked = Signal::derive(move || form.pinned_by(&k()).is_some());
            let pick = Callback::new(move |path: String| {
                let mut next = items.get_untracked();
                if !next.contains(&path) {
                    next.push(path);
                    set(next);
                }
            });
            view! {
                <ListEditor items placeholder="/path/to/music or [Alias]/path"
                    on_change=Callback::new(move |items: Vec<String>| set(items))
                    extra=ViewFn::from(move || view! {
                        <button type="button" class="ui button directory-browse" disabled=locked
                            on:click=move |_| picking.set(true)>
                            <i class="folder open icon" aria-hidden="true"></i>"Browse…"
                        </button>
                    })/>
                <crate::components::directory_picker::DirectoryPicker open=picking on_pick=pick/>
            }
            .into_any()
        }
        Kind::Secret { confirm } => view! {
            <SecretControl key=k() confirm editing=editing_secret/>
        }
        .into_any(),
    };
    view! {
        <Field id=field_id(&key.get_value()) label=label help=help.unwrap_or_default() error=error
            locked_reason=locked tag=tag.unwrap_or_default() actions=actions>
            {control()}
        </Field>
    }
}

/// "Set" / "Not set" with a Change button that reveals a password input (never prefilled).
#[component]
fn SecretControl(key: String, confirm: bool, editing: RwSignal<bool>) -> impl IntoView {
    let form = use_form();
    let key = StoredValue::new(key);
    let k = move || key.get_value();
    let is_set = Signal::derive(move || form.secret_set(&k()));
    let value = RwSignal::new(String::new());
    let again = RwSignal::new(String::new());
    let locked = Signal::derive(move || form.pinned_by(&k()).is_some());
    // Reverting from outside (Undo, Discard, a save) closes the editor.
    Effect::new(move |_| {
        if !form.is_dirty(&k()) && !value.get_untracked().is_empty() {
            value.set(String::new());
            again.set(String::new());
            editing.set(false);
        }
    });
    let check = move || {
        let password = value.get_untracked();
        let problem = if password.is_empty() {
            None
        } else if confirm && password.chars().count() < super::fields::MIN_PASSWORD_LENGTH {
            Some(format!(
                "Use at least {} characters",
                super::fields::MIN_PASSWORD_LENGTH
            ))
        } else if confirm && again.with_untracked(String::is_empty) {
            Some("Repeat the new password below".to_owned())
        } else if confirm && password != again.get_untracked() {
            Some("The passwords do not match".to_owned())
        } else {
            None
        };
        form.set_client_error(&k(), problem);
        if password.is_empty() {
            form.revert(&k());
        } else {
            form.set(&k(), Value::String(password));
        }
    };
    let confirm_id = format!("{}-confirm", field_id(&k()));
    view! {
        <div class="form-secret">
            <span class="ui label form-secret-state">{move || if is_set.get() { "Set" } else { "Not set" }}</span>
            <Show when=move || !editing.get()>
                <button type="button" class="ui small button form-secret-change" disabled=locked
                    on:click=move |_| editing.set(true)>
                    {move || if is_set.get() { "Change" } else { "Set" }}
                </button>
            </Show>
            <Show when=move || editing.get()>
                <button type="button" class="ui small basic button"
                    on:click=move |_| {
                        value.set(String::new());
                        again.set(String::new());
                        editing.set(false);
                        form.revert(&k());
                    }>
                    "Cancel"
                </button>
            </Show>
        </div>
        <Show when=move || editing.get()>
            <PasswordInput value=value placeholder="New value"
                on_input=move |t: String| { value.set(t); check(); }/>
            {confirm.then(|| {
                let confirm_id = confirm_id.clone();
                view! {
                    <label class="form-label form-confirm-label" for=confirm_id.clone()>"Repeat it"</label>
                    <input class="form-input" type="password" id=confirm_id autocomplete="new-password"
                        prop:value=move || again.get()
                        on:input=move |event| { again.set(event_target_value(&event)); check(); }/>
                }
            })}
        </Show>
    }
}
