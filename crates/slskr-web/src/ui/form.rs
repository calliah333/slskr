//! Form building blocks for the setup wizard and the settings page.
//!
//! These components only lay out and wire up native form controls: they hold no business
//! logic (parsing, validation and saving belong to the pages). They emit plain semantic HTML
//! with a few stable class names, all prefixed `form-` (plus `list-editor` and `save-bar`),
//! on top of Semantic UI classes so they match the rest of the UI. Restyling the forms means
//! restyling these components and `assets/static/css/forms.css`.
//!
//! A [`Field`] provides a [`FieldContext`] to the control inside it, so the control picks up
//! the field's `id` (the `<label for>` target), `aria-describedby` (help, lock and error
//! texts), `aria-invalid` and the locked state without repeating them.

use leptos::ev::{Event, KeyboardEvent};
use leptos::html;
use leptos::prelude::*;
use leptos::tachys::reactive_graph::OwnedView;

/// What a [`Field`] tells the control inside it.
#[derive(Clone)]
pub struct FieldContext {
    /// `id` of the control; the field's `<label for>`.
    pub id: String,
    /// The field's label text (for controls that need an accessible name per row).
    pub label: String,
    /// Space-separated ids of the help, lock and error texts currently shown.
    pub described_by: Signal<String>,
    pub invalid: Signal<bool>,
    /// The value is set elsewhere and can not be changed here.
    pub locked: Signal<bool>,
}

fn field_context() -> Option<FieldContext> {
    use_context::<FieldContext>()
}

/// Focuses the first control marked invalid inside `scope` (a CSS selector), after the
/// DOM has been updated.
pub fn focus_first_invalid(scope: &'static str) {
    request_animation_frame(move || {
        let selector = format!("{scope} [aria-invalid=true]");
        if let Ok(Some(element)) = document().query_selector(&selector)
            && let Ok(element) = wasm_bindgen::JsCast::dyn_into::<web_sys::HtmlElement>(element)
        {
            let _ = element.focus();
        }
    });
}

fn target_value(event: &Event) -> String {
    event_target_value(event)
}

/// A titled group of fields: `<section class="ui segment form-section">`.
#[component]
pub fn Section(
    /// Element id, also the anchor other pages link to.
    #[prop(into)]
    id: String,
    #[prop(into)] title: String,
    #[prop(into, optional)] description: Option<String>,
    children: Children,
) -> impl IntoView {
    let title_id = format!("{id}-title");
    view! {
        <section class="ui segment form-section" id=id aria-labelledby=title_id.clone()>
            <h3 class="ui header form-section-title" id=title_id.clone()>{title}</h3>
            {description.map(|text| view! { <p class="form-section-help">{text}</p> })}
            {children()}
        </section>
    }
}

/// Lays out several fields side by side where there is room: `<div class="form-grid">`.
#[component]
pub fn FieldGrid(children: Children) -> impl IntoView {
    view! { <div class="form-grid">{children()}</div> }
}

/// A labelled control with optional help, lock reason and error.
///
/// Renders `<div class="field form-field">` with `error` while an error is shown and
/// `locked` while the value is set elsewhere; the control inside reads the
/// [`FieldContext`].
#[component]
pub fn Field(
    /// `id` of the control.
    #[prop(into)]
    id: String,
    #[prop(into)] label: String,
    #[prop(into, optional)] help: Option<String>,
    #[prop(into, optional)] error: Signal<Option<String>>,
    /// Why the value can not be changed here; the control is disabled while set.
    #[prop(into, optional)]
    locked_reason: Signal<Option<String>>,
    /// A short tag after the label, e.g. "restart".
    #[prop(into, optional)]
    tag: Option<String>,
    /// Buttons after the label (reset, revert).
    #[prop(optional)]
    actions: Option<ViewFn>,
    children: Children,
) -> impl IntoView {
    let help = help.filter(|help| !help.is_empty());
    let tag = tag.filter(|tag| !tag.is_empty());
    let help_id = format!("{id}-help");
    let locked_id = format!("{id}-locked");
    let error_id = format!("{id}-error");
    let label_id = format!("{id}-label");
    let has_help = help.is_some();
    let described_by = Signal::derive({
        let (help_id, locked_id, error_id) = (help_id.clone(), locked_id.clone(), error_id.clone());
        move || {
            let mut ids = Vec::new();
            if has_help {
                ids.push(help_id.as_str());
            }
            if locked_reason.with(Option::is_some) {
                ids.push(locked_id.as_str());
            }
            if error.with(Option::is_some) {
                ids.push(error_id.as_str());
            }
            ids.join(" ")
        }
    });
    let invalid = Signal::derive(move || error.with(Option::is_some));
    let locked = Signal::derive(move || locked_reason.with(Option::is_some));
    // Components share their parent's owner: like `Provider`, give the control an owner of
    // its own that is also current while it renders, so controls rendered later (inside
    // `Show`) find this field's context rather than a sibling's.
    let owner = Owner::current()
        .map(|owner| owner.child())
        .unwrap_or_default();
    let control = owner.with(|| {
        provide_context(FieldContext {
            id: id.clone(),
            label: label.clone(),
            described_by,
            invalid,
            locked,
        });
        children()
    });
    let control = OwnedView::new_with_owner(control, owner);
    view! {
        <div class="field form-field" class:error=invalid class:locked=locked>
            <div class="form-field-head">
                <label for=id.clone() id=label_id class="form-label">{label}</label>
                {tag.map(|tag| view! { <span class="ui mini basic label form-tag">{tag}</span> })}
                {actions.map(|actions| view! { <span class="form-field-actions">{actions.run()}</span> })}
            </div>
            {control}
            {help.map(|text| view! { <p class="form-help" id=help_id>{text}</p> })}
            {move || locked_reason.get().map(|reason| view! {
                <p class="form-locked" id=locked_id.clone()>
                    <i class="lock icon" aria-hidden="true"></i>{reason}
                </p>
            })}
            {move || error.get().map(|message| view! {
                <p class="ui basic red pointing prompt label form-error" id=error_id.clone() role="alert">
                    {message}
                </p>
            })}
        </div>
    }
}

/// A single-line text input inside a [`Field`].
#[component]
pub fn TextInput(
    #[prop(into)] value: Signal<String>,
    #[prop(into)] on_input: Callback<String>,
    #[prop(into, optional)] placeholder: Option<String>,
    #[prop(into, optional)] disabled: Signal<bool>,
    /// `type` attribute; default `text`.
    #[prop(optional)]
    input_type: Option<&'static str>,
    #[prop(optional)] autocomplete: Option<&'static str>,
    #[prop(optional)] inputmode: Option<&'static str>,
    #[prop(into, optional)] name: Option<String>,
    #[prop(optional)] node_ref: Option<NodeRef<html::Input>>,
    /// Called when the value is committed (`change`: blur or Enter).
    #[prop(into, optional)]
    on_change: Option<Callback<String>>,
) -> impl IntoView {
    let field = field_context();
    let id = field.as_ref().map(|f| f.id.clone());
    let described_by = field.as_ref().map(|f| f.described_by);
    let invalid = field.as_ref().map(|f| f.invalid);
    let locked = field.as_ref().map(|f| f.locked);
    let is_disabled = move || disabled.get() || locked.is_some_and(|l| l.get());
    let node_ref = node_ref.unwrap_or_default();
    view! {
        <input
            node_ref=node_ref
            class="form-input"
            type=input_type.unwrap_or("text")
            id=id
            name=name.filter(|name| !name.is_empty())
            autocomplete=autocomplete
            inputmode=inputmode
            spellcheck="false"
            placeholder=placeholder
            aria-describedby=move || described_by.map(|d| d.get()).filter(|d| !d.is_empty())
            aria-invalid=move || invalid.is_some_and(|i| i.get()).then_some("true")
            disabled=is_disabled
            prop:value=move || value.get()
            on:input=move |event| on_input.run(target_value(&event))
            on:change=move |event| {
                if let Some(on_change) = on_change {
                    on_change.run(target_value(&event));
                }
            }
        />
    }
}

/// A password input (never prefilled by the pages).
#[component]
pub fn PasswordInput(
    #[prop(into)] value: Signal<String>,
    #[prop(into)] on_input: Callback<String>,
    #[prop(into, optional)] placeholder: Option<String>,
    #[prop(into, optional)] disabled: Signal<bool>,
    /// Default `new-password`.
    #[prop(optional)]
    autocomplete: Option<&'static str>,
    #[prop(into, optional)] name: Option<String>,
) -> impl IntoView {
    view! {
        <TextInput value on_input placeholder=placeholder.unwrap_or_default() disabled
            name=name.unwrap_or_default() input_type="password"
            autocomplete=autocomplete.unwrap_or("new-password")/>
    }
}

/// A whole-number input. The page parses and validates the text.
#[component]
pub fn NumberInput(
    #[prop(into)] value: Signal<String>,
    #[prop(into)] on_input: Callback<String>,
    #[prop(into, optional)] placeholder: Option<String>,
    #[prop(into, optional)] disabled: Signal<bool>,
) -> impl IntoView {
    view! {
        <TextInput value on_input placeholder=placeholder.unwrap_or_default() disabled inputmode="numeric"/>
    }
}

/// A duration such as `30s`, `5m` or `7d`. The page parses and validates the text.
#[component]
pub fn DurationInput(
    #[prop(into)] value: Signal<String>,
    #[prop(into)] on_input: Callback<String>,
    /// Empty means "never": shown as the placeholder.
    #[prop(optional)]
    optional: bool,
    #[prop(into, optional)] disabled: Signal<bool>,
) -> impl IntoView {
    let placeholder = if optional {
        "never (e.g. 30m, 12h, 7d)"
    } else {
        "e.g. 30s, 5m, 7d"
    };
    view! { <TextInput value on_input disabled placeholder=placeholder/> }
}

/// A multi-line text input.
#[component]
pub fn TextArea(
    #[prop(into)] value: Signal<String>,
    #[prop(into)] on_input: Callback<String>,
    #[prop(into, optional)] placeholder: Option<String>,
    #[prop(into, optional)] disabled: Signal<bool>,
    #[prop(default = 3)] rows: u32,
) -> impl IntoView {
    let field = field_context();
    let id = field.as_ref().map(|f| f.id.clone());
    let described_by = field.as_ref().map(|f| f.described_by);
    let invalid = field.as_ref().map(|f| f.invalid);
    let locked = field.as_ref().map(|f| f.locked);
    view! {
        <textarea
            class="form-input"
            id=id
            rows=rows
            placeholder=placeholder
            aria-describedby=move || described_by.map(|d| d.get()).filter(|d| !d.is_empty())
            aria-invalid=move || invalid.is_some_and(|i| i.get()).then_some("true")
            disabled=move || disabled.get() || locked.is_some_and(|l| l.get())
            prop:value=move || value.get()
            on:input=move |event| on_input.run(target_value(&event))
        ></textarea>
    }
}

/// An on/off switch: `<div class="ui toggle checkbox form-toggle">`.
#[component]
pub fn Toggle(
    #[prop(into)] checked: Signal<bool>,
    #[prop(into)] on_change: Callback<bool>,
    #[prop(into, optional)] disabled: Signal<bool>,
    /// Text shown next to the switch for each state.
    #[prop(optional)]
    states: Option<(&'static str, &'static str)>,
) -> impl IntoView {
    let field = field_context();
    let id = field.as_ref().map(|f| f.id.clone());
    let labelled_by = id.as_ref().map(|id| format!("{id}-label"));
    let described_by = field.as_ref().map(|f| f.described_by);
    let locked = field.as_ref().map(|f| f.locked);
    let is_disabled = move || disabled.get() || locked.is_some_and(|l| l.get());
    let (on, off) = states.unwrap_or(("On", "Off"));
    view! {
        <div class="ui toggle checkbox form-toggle" class:checked=checked class:disabled=is_disabled>
            <input
                type="checkbox"
                role="switch"
                id=id.clone()
                aria-labelledby=labelled_by
                aria-describedby=move || described_by.map(|d| d.get()).filter(|d| !d.is_empty())
                disabled=is_disabled
                prop:checked=move || checked.get()
                on:change=move |event| on_change.run(event_target_checked(&event))
            />
            <label for=id aria-hidden="true">{move || if checked.get() { on } else { off }}</label>
        </div>
    }
}

/// A native `<select>` of `(value, label)` options.
#[component]
pub fn Select(
    #[prop(into)] value: Signal<String>,
    #[prop(into)] on_change: Callback<String>,
    options: Vec<(String, String)>,
    #[prop(into, optional)] disabled: Signal<bool>,
) -> impl IntoView {
    let field = field_context();
    let id = field.as_ref().map(|f| f.id.clone());
    let described_by = field.as_ref().map(|f| f.described_by);
    let invalid = field.as_ref().map(|f| f.invalid);
    let locked = field.as_ref().map(|f| f.locked);
    view! {
        <select
            class="ui dropdown form-select"
            id=id
            aria-describedby=move || described_by.map(|d| d.get()).filter(|d| !d.is_empty())
            aria-invalid=move || invalid.is_some_and(|i| i.get()).then_some("true")
            disabled=move || disabled.get() || locked.is_some_and(|l| l.get())
            prop:value=move || value.get()
            on:change=move |event| on_change.run(target_value(&event))
        >
            {options.into_iter().map(|(option, label)| {
                let selected = {
                    let option = option.clone();
                    move || value.with(|value| *value == option)
                };
                view! { <option value=option selected=selected>{label}</option> }
            }).collect_view()}
        </select>
    }
}

/// Edits a list of strings: one editable row per item with a remove button, and an input
/// that adds an item (Enter or the Add button). Rows edited to blank are removed.
#[component]
pub fn ListEditor(
    #[prop(into)] items: Signal<Vec<String>>,
    #[prop(into)] on_change: Callback<Vec<String>>,
    #[prop(into, optional)] placeholder: Option<String>,
    #[prop(into, optional)] disabled: Signal<bool>,
    /// Text of the add button; default "Add".
    #[prop(into, optional)]
    add_label: Option<String>,
    /// Extra controls after the add button (e.g. a folder picker).
    #[prop(optional)]
    extra: Option<ViewFn>,
) -> impl IntoView {
    let field = field_context();
    let id = field.as_ref().map(|f| f.id.clone());
    let label = field
        .as_ref()
        .map(|f| f.label.clone())
        .unwrap_or_else(|| "Item".to_owned());
    let described_by = field.as_ref().map(|f| f.described_by);
    let invalid = field.as_ref().map(|f| f.invalid);
    let locked = field.as_ref().map(|f| f.locked);
    let is_disabled = Signal::derive(move || disabled.get() || locked.is_some_and(|l| l.get()));
    let pending = RwSignal::new(String::new());
    let add = move || {
        let item = pending.get_untracked().trim().to_owned();
        if item.is_empty() {
            return;
        }
        let mut next = items.get_untracked();
        if !next.contains(&item) {
            next.push(item);
            on_change.run(next);
        }
        pending.set(String::new());
    };
    let edit = move |index: usize, text: String| {
        let mut next = items.get_untracked();
        if let Some(item) = next.get_mut(index) {
            *item = text;
            on_change.run(next);
        }
    };
    let remove = move |index: usize| {
        let mut next = items.get_untracked();
        if index < next.len() {
            next.remove(index);
            on_change.run(next);
        }
    };
    let row_label = label.clone();
    view! {
        <div class="list-editor" role="group" aria-label=label.clone()>
            <ul class="list-editor-items">
                <For each=move || 0..items.with(Vec::len) key=|index| *index let:index>
                    {
                        let row_value = move || items.with(|items| items.get(index).cloned().unwrap_or_default());
                        let aria = format!("{} {}", row_label, index + 1);
                        view! {
                            <li class="list-editor-item">
                                <div class="ui fluid action input">
                                    <input class="form-input" type="text" spellcheck="false" aria-label=aria
                                        disabled=is_disabled
                                        prop:value=row_value
                                        on:input=move |event| edit(index, target_value(&event))
                                        on:change=move |event| {
                                            if target_value(&event).trim().is_empty() {
                                                remove(index);
                                            }
                                        }/>
                                    <button type="button" class="ui icon button list-editor-remove"
                                        disabled=is_disabled
                                        aria-label=move || format!("Remove {}", row_value())
                                        on:click=move |_| remove(index)>
                                        <i class="close icon" aria-hidden="true"></i>
                                    </button>
                                </div>
                            </li>
                        }
                    }
                </For>
            </ul>
            <div class="list-editor-add">
                <div class="ui fluid action input">
                    <input class="form-input" type="text" spellcheck="false" id=id
                        placeholder=placeholder
                        aria-describedby=move || described_by.map(|d| d.get()).filter(|d| !d.is_empty())
                        aria-invalid=move || invalid.is_some_and(|i| i.get()).then_some("true")
                        disabled=is_disabled
                        prop:value=move || pending.get()
                        on:input=move |event| pending.set(target_value(&event))
                        on:keydown=move |event: KeyboardEvent| {
                            if event.key() == "Enter" {
                                // Adding the item, not submitting the surrounding form.
                                event.prevent_default();
                                add();
                            }
                        }/>
                    <button type="button" class="ui button list-editor-add-button"
                        disabled=move || is_disabled.get() || pending.with(|p| p.trim().is_empty())
                        on:click=move |_| add()>
                        {add_label.unwrap_or_else(|| "Add".to_owned())}
                    </button>
                </div>
                {extra.map(|extra| extra.run())}
            </div>
        </div>
    }
}

/// A status or error message: `<div class="ui {kind} message form-message">`.
#[component]
pub fn FormMessage(
    /// Semantic UI message kind: `negative`, `positive`, `warning` or `info`.
    kind: &'static str,
    /// `alert` for errors, `status` for everything else.
    #[prop(optional)]
    role: Option<&'static str>,
    children: Children,
) -> impl IntoView {
    let role = role.unwrap_or(if kind == "negative" {
        "alert"
    } else {
        "status"
    });
    view! { <div class=format!("ui {kind} message form-message") role=role>{children()}</div> }
}

/// The bar with the number of unsaved changes and the Save and Discard buttons, fixed at
/// the bottom of the window (rendered into `<body>`, outside the app's clipping layout).
/// Hidden while nothing changed.
#[component]
pub fn SaveBar(
    #[prop(into)] dirty: Signal<usize>,
    #[prop(into)] saving: Signal<bool>,
    #[prop(into)] on_save: Callback<()>,
    #[prop(into)] on_discard: Callback<()>,
    /// A short problem shown in the bar (e.g. invalid fields).
    #[prop(into, optional)]
    problem: Signal<Option<String>>,
) -> impl IntoView {
    view! {
        <Show when={move || dirty.get() > 0}>
            <leptos::portal::Portal>
            <div class="save-bar" role="region" aria-label="Unsaved changes">
                <span class="save-bar-count" role="status">
                    {move || match dirty.get() {
                        1 => "1 unsaved change".to_owned(),
                        n => format!("{n} unsaved changes"),
                    }}
                </span>
                {move || problem.get().map(|text| view! { <span class="save-bar-problem">{text}</span> })}
                <span class="save-bar-buttons">
                    <button type="button" class="ui button" disabled=saving on:click=move |_| on_discard.run(())>
                        "Discard"
                    </button>
                    <button type="button" class="ui primary button" class:loading=saving disabled=saving
                        on:click=move |_| on_save.run(())>
                        "Save"
                    </button>
                </span>
            </div>
            </leptos::portal::Portal>
        </Show>
    }
}
