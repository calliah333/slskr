//! A settings row bound to a dotted key of the [`Form`].

use leptos::prelude::*;
use serde_json::Value;

use super::model::{Form, Parse, field_id, text_of};
use crate::ui::Icon;
use crate::ui::form::{
    ChipInput, DurationInput, Field, NumberInput, PasswordInput, Select, TextArea, TextInput,
    Toggle,
};

/// The control of a [`Setting`].
#[derive(Clone)]
pub enum Kind {
    Text,
    /// Monospace text: paths, addresses, patterns.
    Mono,
    TextArea,
    /// A whole number in `min..=max`, with a unit after the input. With `zero` a checkbox
    /// of that name stands for 0 (e.g. "Unlimited"); unchecking it sets `fallback`.
    Number {
        min: u64,
        max: u64,
        unit: Option<&'static str>,
        zero: Option<&'static str>,
        fallback: u64,
    },
    Duration,
    /// A duration where empty means never.
    OptionalDuration,
    Toggle,
    Select(Vec<(&'static str, &'static str)>),
    /// A list of strings as chips; the text is the input's placeholder.
    List(&'static str),
    /// [`Kind::List`] with monospace chips (patterns, addresses).
    MonoList(&'static str),
    /// A write-only value; `confirm` asks for it twice.
    Secret {
        confirm: bool,
    },
}

impl Kind {
    /// A count from 0 with an optional unit.
    pub const fn count(unit: Option<&'static str>) -> Self {
        Self::Number {
            min: 0,
            max: u32::MAX as u64,
            unit,
            zero: None,
            fallback: 1,
        }
    }

    /// A count where 0 means "Unlimited" (or another `zero` label).
    pub const fn zero_means(unit: Option<&'static str>, zero: &'static str, fallback: u64) -> Self {
        Self::Number {
            min: 0,
            max: u32::MAX as u64,
            unit,
            zero: Some(zero),
            fallback,
        }
    }

    pub const fn unlimited(unit: Option<&'static str>, fallback: u64) -> Self {
        Self::zero_means(unit, "Unlimited", fallback)
    }

    /// A count from 1.
    pub const fn at_least_one(unit: Option<&'static str>) -> Self {
        Self::Number {
            min: 1,
            max: u32::MAX as u64,
            unit,
            zero: None,
            fallback: 1,
        }
    }

    /// Controls that need the full width go below the label.
    fn wide(&self) -> bool {
        matches!(self, Self::TextArea | Self::List(_) | Self::MonoList(_))
    }
}

pub fn use_form() -> Form {
    expect_context()
}

/// Minimum length of a web password set here (as in first-run setup).
pub const MIN_PASSWORD_LENGTH: usize = 8;

/// The environment variable that sets the dotted `key` (`web.listen` → `SLSKR_WEB_LISTEN`).
pub fn env_var(key: &str) -> String {
    format!("SLSKR_{}", key.replace('.', "_").to_ascii_uppercase())
}

/// Why a pinned key can not be changed here.
pub fn lock_reason(pinned: &str) -> String {
    format!("Set in slskr.toml or {} ({pinned})", env_var(pinned))
}

/// Deletes the stored value of `key` (back to its default).
pub fn reset(form: Form, key: String) {
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
}

/// The small Undo / Reset buttons of a row.
pub fn row_actions(form: Form, key: StoredValue<String>, on_undo: Callback<()>) -> ViewFn {
    let k = move || key.get_value();
    ViewFn::from(move || {
        view! {
            <Show when=move || form.is_dirty(&k())>
                <button type="button" class="btn btn--action btn--xs form-revert"
                    aria-label="Undo change" title="Undo change"
                    on:click=move |_| { on_undo.run(()); form.revert(&k()); }>
                    <Icon name="undo-2" size="sm"/>
                </button>
            </Show>
            <Show when=move || !form.is_dirty(&k()) && form.is_stored(&k()) && form.pinned_by(&k()).is_none()>
                <button type="button" class="btn btn--action btn--xs form-reset"
                    aria-label="Reset to default" title="Reset to default"
                    on:click=move |_| reset(form, k())>
                    <Icon name="rotate-ccw" size="sm"/>
                </button>
            </Show>
        }
    })
}

/// One setting row: label and help on the left, the control on the right; the lock when
/// pinned, errors, Undo and Reset.
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
    let locked = Signal::derive(move || form.pinned_by(&k()).map(|pinned| lock_reason(&pinned)));
    let error = Signal::derive(move || form.error(&k()));
    let editing_secret = RwSignal::new(false);
    let actions = row_actions(
        form,
        key,
        Callback::new(move |()| editing_secret.set(false)),
    );
    let wide = kind.wide();
    let text = Signal::derive(move || form.text(&k()));
    let placeholder = placeholder.unwrap_or_default();
    let list = move || {
        Signal::derive(move || {
            form.value(&k())
                .as_array()
                .map(|items| items.iter().map(text_of).collect())
                .unwrap_or_default()
        })
    };
    let set_list = Callback::new(move |items: Vec<String>| {
        form.set(
            &k(),
            Value::Array(items.into_iter().map(Value::String).collect()),
        );
    });
    // Built inside the Field, so the control finds its FieldContext.
    let control = move || match kind {
        Kind::Text | Kind::Mono => view! {
            <TextInput value=text placeholder=placeholder.clone() mono=matches!(kind, Kind::Mono)
                on_input=move |t: String| form.input(&k(), Parse::Text, t)/>
        }
        .into_any(),
        Kind::TextArea => view! {
            <TextArea value=text placeholder=placeholder.clone()
                on_input=move |t: String| form.input(&k(), Parse::Text, t)/>
        }
        .into_any(),
        Kind::Number {
            min,
            max,
            unit,
            zero,
            fallback,
        } => view! {
            <NumberInput value=text placeholder=placeholder.clone()
                unit=unit.map(str::to_owned) zero_label=zero.map(str::to_owned)
                fallback=fallback.to_string()
                on_input=move |t: String| form.input(&k(), Parse::Number { min, max }, t)/>
        }
        .into_any(),
        Kind::Duration | Kind::OptionalDuration => {
            let optional = matches!(kind, Kind::OptionalDuration);
            view! {
                <DurationInput value=Signal::derive(move || form.duration_text(&k())) optional=optional
                    on_input=move |t: String| form.input(&k(), Parse::Duration { optional }, t)/>
            }
            .into_any()
        }
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
        Kind::List(placeholder) => view! {
            <ChipInput items=list() placeholder=placeholder on_change=set_list/>
        }
        .into_any(),
        Kind::MonoList(placeholder) => view! {
            <ChipInput items=list() placeholder=placeholder mono=true on_change=set_list/>
        }
        .into_any(),
        Kind::Secret { confirm } => view! {
            <SecretControl key=k() confirm editing=editing_secret/>
        }
        .into_any(),
    };
    let (tag, tag_title) = if restart {
        (
            "Restart".to_owned(),
            "Takes effect after slskr restarts".to_owned(),
        )
    } else {
        (String::new(), String::new())
    };
    view! {
        <Field id=field_id(&key.get_value()) label=label help=help.unwrap_or_default() error=error
            locked_reason=locked tag=tag tag_title=tag_title actions=actions row=true wide=wide>
            {control()}
        </Field>
    }
}

/// A write-only value: dots (or "Not set") and a Change button that opens password inputs
/// (never prefilled); `confirm` asks for the new value twice.
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
        } else if confirm && password.chars().count() < MIN_PASSWORD_LENGTH {
            Some(format!("Use at least {MIN_PASSWORD_LENGTH} characters"))
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
    let open = move |_| {
        editing.set(true);
        let id = field_id(&k());
        request_animation_frame(move || {
            if let Some(input) = document().get_element_by_id(&id)
                && let Ok(input) = wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(input)
            {
                let _ = input.focus();
            }
        });
    };
    view! {
        <Show when=move || !editing.get()>
            <div class="form-secret">
                <span class="form-secret-state" class:is-set=is_set
                    data-set=move || if is_set.get() { "true" } else { "false" }>
                    {move || if is_set.get() { "••••••••" } else { "Not set" }}
                </span>
                <button type="button" class="btn btn--sm form-secret-change" disabled=locked on:click=open>
                    <Icon name="key"/>
                    {move || if is_set.get() { "Change" } else { "Set" }}
                </button>
            </div>
        </Show>
        <Show when=move || editing.get()>
            <div class="form-secret-edit">
                <PasswordInput value=value placeholder=if confirm { "New password" } else { "New value" }
                    on_input=move |t: String| { value.set(t); check(); }/>
                {confirm.then(|| {
                    let confirm_id = confirm_id.clone();
                    view! {
                        <input class="input form-input form-confirm" type="password" id=confirm_id
                            autocomplete="new-password" placeholder="Repeat new password"
                            aria-label="Repeat new password"
                            prop:value=move || again.get()
                            on:input=move |event| { again.set(event_target_value(&event)); check(); }/>
                    }
                })}
                <button type="button" class="btn btn--sm btn--ghost form-secret-cancel"
                    on:click=move |_| {
                        value.set(String::new());
                        again.set(String::new());
                        editing.set(false);
                        form.revert(&k());
                    }>
                    "Cancel"
                </button>
            </div>
        </Show>
    }
}
