//! Form building blocks for the setup wizard and the settings page.
//!
//! These components only lay out and wire up native form controls: they hold no business
//! logic (parsing, validation and saving belong to the pages). They emit plain semantic HTML
//! with the design-system classes (`card`, `field`, `input`, `switch`, …) plus a few stable
//! hook classes prefixed `form-` (and `chip-input`, `save-bar`) that tests and page CSS
//! select on. Their look is `assets/static/css/pages/settings.css`.
//!
//! A [`Field`] provides a [`FieldContext`] to the control inside it, so the control picks up
//! the field's `id` (the `<label for>` target), `aria-describedby` (help, lock and error
//! texts), `aria-invalid` and the locked state without repeating them.
//!
//! Two layouts: stacked (label, control, help; the setup wizard and dialogs) and `row`
//! (label and help on the left, a right-sized control on the right; settings). A `wide`
//! row puts a control that needs the width (lists, text areas) below its label.

use leptos::ev::{Event, KeyboardEvent};
use leptos::html;
use leptos::prelude::*;
use leptos::tachys::reactive_graph::OwnedView;
use slskr_types::config::duration;

use super::feedback::Alert;
use super::icon::Icon;

/// What a [`Field`] tells the control inside it.
#[derive(Clone)]
pub struct FieldContext {
    /// `id` of the control; the field's `<label for>`.
    pub id: String,
    /// The field's label text (for controls that need an accessible name per part).
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

/// The parts of the [`FieldContext`] a native control needs.
struct Bound {
    id: Option<String>,
    label: String,
    described_by: Option<Signal<String>>,
    invalid: Option<Signal<bool>>,
    locked: Option<Signal<bool>>,
}

impl Bound {
    fn get() -> Self {
        let field = field_context();
        Self {
            id: field.as_ref().map(|f| f.id.clone()),
            label: field
                .as_ref()
                .map_or_else(|| "Value".to_owned(), |f| f.label.clone()),
            described_by: field.as_ref().map(|f| f.described_by),
            invalid: field.as_ref().map(|f| f.invalid),
            locked: field.as_ref().map(|f| f.locked),
        }
    }

    fn describedby(&self) -> impl Fn() -> Option<String> + Send + Sync + 'static + use<> {
        let described_by = self.described_by;
        move || described_by.map(|d| d.get()).filter(|d| !d.is_empty())
    }

    fn aria_invalid(&self) -> impl Fn() -> Option<&'static str> + Send + Sync + 'static + use<> {
        let invalid = self.invalid;
        move || invalid.is_some_and(|i| i.get()).then_some("true")
    }

    fn is_locked(&self) -> impl Fn() -> bool + Send + Sync + Copy + 'static + use<> {
        let locked = self.locked;
        move || locked.is_some_and(|l| l.get())
    }
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

/// A titled card of setting rows: `<section class="card form-section">`.
#[component]
pub fn Section(
    /// Element id, also the anchor other pages link to.
    #[prop(into)]
    id: String,
    #[prop(into)] title: String,
    #[prop(into, optional)] description: Option<String>,
    /// Buttons (or a status badge) at the right of the header.
    #[prop(optional)]
    actions: Option<ViewFn>,
    /// A `card__footer` below the rows.
    #[prop(optional)]
    footer: Option<ViewFn>,
    children: Children,
) -> impl IntoView {
    let title_id = format!("{id}-title");
    view! {
        <section class="card form-section" id=id aria-labelledby=title_id.clone()>
            <div class="card__header">
                <div class="form-section-head">
                    <h2 class="card__title form-section-title" id=title_id.clone()>{title}</h2>
                    {description.map(|text| view! { <p class="card__description form-section-help">{text}</p> })}
                </div>
                {actions.map(|actions| view! { <div class="card__actions">{actions.run()}</div> })}
            </div>
            <div class="card__content form-rows">{children()}</div>
            {footer.map(|footer| view! { <div class="card__footer form-section-footer">{footer.run()}</div> })}
        </section>
    }
}

/// A small heading between the rows of a [`Section`].
#[component]
pub fn RowHeading(
    #[prop(into)] title: String,
    #[prop(into, optional)] description: Option<String>,
) -> impl IntoView {
    view! {
        <div class="form-rows-heading">
            <h3>{title}</h3>
            {description.map(|text| view! { <p class="help">{text}</p> })}
        </div>
    }
}

/// Lays out several stacked fields side by side where there is room.
#[component]
pub fn FieldGrid(children: Children) -> impl IntoView {
    view! { <div class="form-grid">{children()}</div> }
}

/// A labelled control with optional help, lock reason and error.
///
/// Renders `<div class="field form-field">` (+ `form-row` in the row layout) with
/// `field--error` while an error is shown and `field--locked` while the value is set
/// elsewhere. A locked field shows a lock icon after the label whose tooltip (and
/// screen-reader text, `.form-locked`) is the reason; the control is disabled.
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
    /// A short tag after the label, e.g. "Restart".
    #[prop(into, optional)]
    tag: Option<String>,
    /// Tooltip of the tag.
    #[prop(into, optional)]
    tag_title: Option<String>,
    /// Small buttons after the label (reset, revert).
    #[prop(optional)]
    actions: Option<ViewFn>,
    /// Label and help on the left, the control on the right.
    #[prop(optional)]
    row: bool,
    /// In a row: the control takes the full width below the label.
    #[prop(optional)]
    wide: bool,
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
    let head = view! {
        <div class="form-field-head">
            <label for=id.clone() id=label_id class="label form-label">{label}</label>
            {move || locked_reason.get().map(|reason| { let title = reason.clone(); view! {
                <span class="form-locked" id=locked_id.clone() title=title>
                    <Icon name="lock" size="xs"/>
                    <span class="sr-only">{reason}</span>
                </span> }
            })}
            {tag.map(|tag| view! { <span class="badge badge--outline form-tag" title=tag_title>{tag}</span> })}
            {actions.map(|actions| view! { <span class="form-field-actions">{actions.run()}</span> })}
        </div>
    };
    let help = help.map(|text| view! { <p class="help form-help" id=help_id>{text}</p> });
    let error = move || {
        error.get().map(|message| {
            view! {
                <p class="error form-error" id=error_id.clone() role="alert">
                    <Icon name="circle-alert" size="xs"/>{message}
                </p>
            }
        })
    };
    let class = match (row, wide) {
        (true, true) => "field form-field form-row form-row--wide",
        (true, false) => "field form-field form-row",
        _ => "field form-field",
    };
    if row {
        view! {
            <div class=class class:field--error=invalid class:field--locked=locked>
                <div class="form-row__text">{head}{help}</div>
                <div class="form-row__control">{control}{error}</div>
            </div>
        }
        .into_any()
    } else {
        view! {
            <div class=class class:field--error=invalid class:field--locked=locked>
                {head}{control}{help}{error}
            </div>
        }
        .into_any()
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
    /// Monospace text (paths, addresses).
    #[prop(optional)]
    mono: bool,
    /// Called when the value is committed (`change`: blur or Enter).
    #[prop(into, optional)]
    on_change: Option<Callback<String>>,
) -> impl IntoView {
    let bound = Bound::get();
    let is_locked = bound.is_locked();
    let node_ref = node_ref.unwrap_or_default();
    view! {
        <input
            node_ref=node_ref
            class=if mono { "input input--mono form-input" } else { "input form-input" }
            type=input_type.unwrap_or("text")
            id=bound.id.clone()
            name=name.filter(|name| !name.is_empty())
            autocomplete=autocomplete
            inputmode=inputmode
            spellcheck="false"
            placeholder=placeholder
            aria-describedby=bound.describedby()
            aria-invalid=bound.aria_invalid()
            disabled=move || disabled.get() || is_locked()
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

/// A narrow whole-number input with an optional unit after it ("KiB/s", "files").
///
/// With `zero_label` a checkbox next to it means 0 ("Unlimited"): checked, the input is
/// empty and disabled; unchecking it puts `fallback` back. The page parses the text.
#[component]
pub fn NumberInput(
    #[prop(into)] value: Signal<String>,
    #[prop(into)] on_input: Callback<String>,
    #[prop(into, optional)] placeholder: Option<String>,
    #[prop(into, optional)] disabled: Signal<bool>,
    #[prop(default = None)] unit: Option<String>,
    /// Label of the "0" checkbox, e.g. "Unlimited".
    #[prop(default = None)]
    zero_label: Option<String>,
    /// Text set when the "0" checkbox is unchecked.
    #[prop(into, optional)]
    fallback: Option<String>,
) -> impl IntoView {
    let bound = Bound::get();
    let is_locked = bound.is_locked();
    let has_zero = zero_label.is_some();
    let is_zero = move || has_zero && value.with(|v| v.trim() == "0");
    let fallback = fallback.unwrap_or_else(|| "1".to_owned());
    let zero_aria = format!(
        "{}: {}",
        bound.label,
        zero_label.clone().unwrap_or_default()
    );
    let placeholder_text = zero_label.clone();
    let input = view! {
        <input
            class="input form-input form-number"
            type="text"
            id=bound.id.clone()
            inputmode="numeric"
            spellcheck="false"
            autocomplete="off"
            placeholder=move || if is_zero() { placeholder_text.clone() } else { placeholder.clone() }
            aria-describedby=bound.describedby()
            aria-invalid=bound.aria_invalid()
            disabled=move || disabled.get() || is_locked() || is_zero()
            prop:value=move || if is_zero() { String::new() } else { value.get() }
            on:input=move |event| on_input.run(target_value(&event))
        />
    };
    view! {
        <div class="form-number-wrap">
            {match unit {
                Some(unit) => view! {
                    <div class="input-affix">
                        {input}
                        <span class="input-affix__unit" aria-hidden="true">{unit}</span>
                    </div>
                }.into_any(),
                None => input.into_any(),
            }}
            {zero_label.map(|text| view! {
                <label class="check form-zero">
                    <input type="checkbox" class="checkbox" aria-label=zero_aria
                        disabled=move || disabled.get() || is_locked()
                        prop:checked=is_zero
                        on:change=move |event| {
                            on_input.run(if event_target_checked(&event) { "0".to_owned() } else { fallback.clone() });
                        }/>
                    <span aria-hidden="true">{text}</span>
                </label>
            })}
        </div>
    }
}

/// Units offered by [`DurationInput`]: `(suffix, name)`, largest last.
const DURATION_UNITS: [(&str, &str); 5] = [
    ("s", "seconds"),
    ("m", "minutes"),
    ("h", "hours"),
    ("d", "days"),
    ("w", "weeks"),
];
const NEVER: &str = "never";

/// Splits duration text into a whole number and one unit: `"90m"` gives `("90", "m")`,
/// `"2h"` `("2", "h")`, `"4w2d"` `("30", "d")`. Text the picker itself wrote (a typed
/// number and a unit) splits back into those. `None` for empty text ("never").
fn split_duration(text: &str) -> Option<(String, String)> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(parsed) = duration::parse(text) {
        let ms = parsed.as_millis();
        for (suffix, _) in DURATION_UNITS.iter().rev() {
            let size: u128 = match *suffix {
                "w" => 604_800_000,
                "d" => 86_400_000,
                "h" => 3_600_000,
                "m" => 60_000,
                _ => 1_000,
            };
            if ms > 0 && ms % size == 0 {
                return Some(((ms / size).to_string(), (*suffix).to_owned()));
            }
        }
        if ms == 0 {
            return Some(("0".to_owned(), "s".to_owned()));
        }
        return Some((ms.to_string(), "ms".to_owned()));
    }
    // Not a valid duration: what was typed, followed by the unit picked.
    for suffix in ["ms", "s", "m", "h", "d", "w"] {
        if let Some(number) = text.strip_suffix(suffix) {
            return Some((number.to_owned(), suffix.to_owned()));
        }
    }
    Some((text.to_owned(), "m".to_owned()))
}

/// A duration as a number and a unit (seconds … weeks). With `optional` the unit list ends
/// with "Never", which hides the number and means empty text.
///
/// `value` is duration text (`"7d"`, `""`); `on_input` gets the number and the unit
/// suffix joined (`"12h"`, `""`), which the page parses.
#[component]
pub fn DurationInput(
    #[prop(into)] value: Signal<String>,
    #[prop(into)] on_input: Callback<String>,
    /// Offer "Never" (empty text).
    #[prop(optional)]
    optional: bool,
    #[prop(into, optional)] disabled: Signal<bool>,
) -> impl IntoView {
    let bound = Bound::get();
    let is_locked = bound.is_locked();
    let initial = split_duration(&value.get_untracked());
    let number = RwSignal::new(initial.as_ref().map(|(n, _)| n.clone()).unwrap_or_default());
    let unit = RwSignal::new(initial.map(|(_, u)| u).unwrap_or_else(|| NEVER.to_owned()));
    let joined = move || {
        let unit = unit.get_untracked();
        if unit == NEVER {
            String::new()
        } else {
            format!("{}{unit}", number.get_untracked().trim())
        }
    };
    // Follow outside changes (discard, reset, live updates) but not our own echo.
    Effect::new(move |_| {
        let text = value.get();
        if text.trim() == joined() {
            return;
        }
        let same = matches!(
            (duration::parse(text.trim()), duration::parse(&joined())),
            (Ok(a), Ok(b)) if a == b
        );
        if same {
            return;
        }
        match split_duration(&text) {
            Some((n, u)) => {
                number.set(n);
                unit.set(u);
            }
            None => unit.set(NEVER.to_owned()),
        }
    });
    let never = move || unit.with(|u| u == NEVER);
    let select_id = bound.id.as_ref().map(|id| format!("{id}-unit"));
    let unit_label = format!("{} unit", bound.label);
    let options = DURATION_UNITS
        .iter()
        .map(|(suffix, name)| ((*suffix).to_owned(), (*name).to_owned()))
        .collect::<Vec<_>>();
    view! {
        <div class="form-duration">
            <input
                class="input form-input form-number"
                type="text"
                id=bound.id.clone()
                inputmode="numeric"
                spellcheck="false"
                autocomplete="off"
                hidden=never
                aria-describedby=bound.describedby()
                aria-invalid=bound.aria_invalid()
                disabled=move || disabled.get() || is_locked()
                prop:value=move || number.get()
                on:input=move |event| {
                    number.set(target_value(&event));
                    on_input.run(joined());
                }
            />
            <select
                class="select form-select form-duration-unit"
                id=select_id
                aria-label=unit_label
                aria-describedby=bound.describedby()
                disabled=move || disabled.get() || is_locked()
                prop:value=move || unit.get()
                on:change=move |event| {
                    let picked = target_value(&event);
                    if picked != NEVER && number.with_untracked(|n| n.trim().is_empty()) {
                        number.set("1".to_owned());
                    }
                    unit.set(picked);
                    on_input.run(joined());
                }
            >
                {move || unit.with(|u| u == "ms").then(|| view! { <option value="ms">"milliseconds"</option> })}
                {options.into_iter().map(|(suffix, name)| {
                    let selected = {
                        let suffix = suffix.clone();
                        move || unit.with(|u| *u == suffix)
                    };
                    view! { <option value=suffix selected=selected>{name}</option> }
                }).collect_view()}
                {optional.then(|| view! { <option value=NEVER selected=never>"Never"</option> })}
            </select>
        </div>
    }
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
    let bound = Bound::get();
    let is_locked = bound.is_locked();
    view! {
        <textarea
            class="textarea form-input"
            id=bound.id.clone()
            rows=rows
            placeholder=placeholder
            aria-describedby=bound.describedby()
            aria-invalid=bound.aria_invalid()
            disabled=move || disabled.get() || is_locked()
            prop:value=move || value.get()
            on:input=move |event| on_input.run(target_value(&event))
        ></textarea>
    }
}

/// An on/off switch: `<label class="switch form-toggle">`, optionally with state text.
#[component]
pub fn Toggle(
    #[prop(into)] checked: Signal<bool>,
    #[prop(into)] on_change: Callback<bool>,
    #[prop(into, optional)] disabled: Signal<bool>,
    /// Text shown next to the switch for each state.
    #[prop(optional)]
    states: Option<(&'static str, &'static str)>,
) -> impl IntoView {
    let bound = Bound::get();
    let is_locked = bound.is_locked();
    let labelled_by = bound.id.as_ref().map(|id| format!("{id}-label"));
    view! {
        <label class="switch form-toggle">
            <input
                type="checkbox"
                role="switch"
                id=bound.id.clone()
                aria-labelledby=labelled_by
                aria-describedby=bound.describedby()
                disabled=move || disabled.get() || is_locked()
                prop:checked=move || checked.get()
                on:change=move |event| on_change.run(event_target_checked(&event))
            />
            <span class="switch__track" aria-hidden="true"></span>
            {states.map(|(on, off)| view! {
                <span class="switch__label" aria-hidden="true">{move || if checked.get() { on } else { off }}</span>
            })}
        </label>
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
    let bound = Bound::get();
    let is_locked = bound.is_locked();
    view! {
        <select
            class="select form-select"
            id=bound.id.clone()
            aria-describedby=bound.describedby()
            aria-invalid=bound.aria_invalid()
            disabled=move || disabled.get() || is_locked()
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

/// Edits a list of strings as chips: each item is a chip with a remove button; typing and
/// Enter (or leaving the input) adds one, Backspace in the empty input removes the last.
#[component]
pub fn ChipInput(
    #[prop(into)] items: Signal<Vec<String>>,
    #[prop(into)] on_change: Callback<Vec<String>>,
    #[prop(into, optional)] placeholder: Option<String>,
    #[prop(into, optional)] disabled: Signal<bool>,
    /// Monospace chips (patterns, addresses).
    #[prop(optional)]
    mono: bool,
) -> impl IntoView {
    let bound = Bound::get();
    let is_locked = bound.is_locked();
    let is_disabled = Signal::derive(move || disabled.get() || is_locked());
    let pending = RwSignal::new(String::new());
    let input_ref = NodeRef::<html::Input>::new();
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
    let remove = move |index: usize| {
        let mut next = items.get_untracked();
        if index < next.len() {
            next.remove(index);
            on_change.run(next);
        }
    };
    let label = bound.label.clone();
    let invalid = bound.aria_invalid();
    let class = if mono {
        "chip-input chip-input--mono"
    } else {
        "chip-input"
    };
    view! {
        <div class=class role="group" aria-label=label.clone()
            class:is-disabled=is_disabled
            class:is-invalid=move || invalid().is_some()
            on:click=move |_| {
                if let Some(input) = input_ref.get_untracked() {
                    let _ = input.focus();
                }
            }>
            <ul class="chip-input__chips" aria-label=format!("{label}: entries")>
                <For each=move || items.get().into_iter().enumerate() key=|(index, item)| (*index, item.clone()) let:entry>
                    {
                        let (index, item) = entry;
                        view! {
                            <li class="chip">
                                <span class="chip__text">{item.clone()}</span>
                                <button type="button" class="chip__remove" disabled=is_disabled
                                    aria-label=format!("Remove {item}") title="Remove"
                                    on:click=move |event| { event.stop_propagation(); remove(index); }>
                                    <Icon name="x" size="xs"/>
                                </button>
                            </li>
                        }
                    }
                </For>
            </ul>
            <input
                node_ref=input_ref
                class="chip-input__input form-input"
                type="text"
                spellcheck="false"
                autocomplete="off"
                id=bound.id.clone()
                placeholder=placeholder
                aria-describedby=bound.describedby()
                aria-invalid=bound.aria_invalid()
                disabled=is_disabled
                prop:value=move || pending.get()
                on:input=move |event| pending.set(target_value(&event))
                on:blur=move |_| add()
                on:keydown=move |event: KeyboardEvent| match event.key().as_str() {
                    "Enter" => {
                        // Adding the item, not submitting the surrounding form.
                        event.prevent_default();
                        add();
                    }
                    "Backspace" if pending.with_untracked(String::is_empty) => {
                        let count = items.with_untracked(Vec::len);
                        if count > 0 {
                            remove(count - 1);
                        }
                    }
                    _ => {}
                }
            />
        </div>
    }
}

/// "Show advanced": a disclosure with rarely changed rows. Opens by itself while `open`
/// is true (e.g. a row inside has an error).
#[component]
pub fn Advanced(
    #[prop(into, optional)] open: Signal<bool>,
    /// Summary text; default "Advanced settings".
    #[prop(into, optional)]
    label: Option<String>,
    children: Children,
) -> impl IntoView {
    let details = NodeRef::<html::Details>::new();
    Effect::new(move |_| {
        if open.get()
            && let Some(details) = details.get()
        {
            let _ = details.set_attribute("open", "");
        }
    });
    let label = label.unwrap_or_else(|| "Advanced settings".to_owned());
    view! {
        <details class="form-advanced" node_ref=details>
            <summary class="form-advanced__summary">
                <Icon name="chevron-right" class="form-advanced__chevron"/>
                <span class="form-advanced__show">"Show "{label.to_lowercase()}</span>
                <span class="form-advanced__hide">"Hide "{label.to_lowercase()}</span>
            </summary>
            <div class="form-advanced__body form-rows">{children()}</div>
        </details>
    }
}

/// A status or error message: an [`Alert`] with the `form-message` hook class.
#[component]
pub fn FormMessage(
    /// `danger` (or `negative`), `success` (or `positive`), `warning` or `info`.
    kind: &'static str,
    /// `alert` for errors, `status` for everything else.
    #[prop(optional)]
    role: Option<&'static str>,
    #[prop(into, optional)] title: Option<String>,
    children: Children,
) -> impl IntoView {
    let title = title.unwrap_or_default();
    match role {
        Some(role) => view! { <Alert variant=kind role=role title=title class="form-message">{children()}</Alert> }
            .into_any(),
        None => view! { <Alert variant=kind title=title class="form-message">{children()}</Alert> }.into_any(),
    }
}

/// The bar with the number of unsaved changes and the Discard and Save buttons, fixed at
/// the bottom of the window (rendered into `<body>`, outside the app's clipping layout).
/// Slides in while there are changes.
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
                <span class="save-bar-dot" aria-hidden="true"></span>
                <div class="save-bar-text">
                    <span class="save-bar-count" role="status">
                        {move || match dirty.get() {
                            1 => "1 unsaved change".to_owned(),
                            n => format!("{n} unsaved changes"),
                        }}
                    </span>
                    {move || problem.get().map(|text| view! { <span class="save-bar-problem">{text}</span> })}
                </div>
                <span class="save-bar-buttons">
                    <button type="button" class="btn btn--ghost" disabled=saving on:click=move |_| on_discard.run(())>
                        "Discard"
                    </button>
                    <button type="button" class="btn btn--primary" disabled=saving aria-busy=move || saving.get().then_some("true")
                        on:click=move |_| on_save.run(())>
                        {move || if saving.get() {
                            view! { <span class="spinner" aria-hidden="true"></span> }.into_any()
                        } else {
                            view! { <Icon name="check"/> }.into_any()
                        }}
                        "Save changes"
                    </button>
                </span>
            </div>
            </leptos::portal::Portal>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::split_duration;

    fn split(text: &str) -> Option<(String, String)> {
        split_duration(text)
    }

    #[test]
    fn durations_split_into_number_and_unit() {
        assert_eq!(split(""), None);
        assert_eq!(split("5s"), Some(("5".into(), "s".into())));
        assert_eq!(split("90m"), Some(("90".into(), "m".into())));
        assert_eq!(split("1h30m"), Some(("90".into(), "m".into())));
        assert_eq!(split("4w2d"), Some(("30".into(), "d".into())));
        assert_eq!(split("2w"), Some(("2".into(), "w".into())));
        assert_eq!(split("1500ms"), Some(("1500".into(), "ms".into())));
        // What the picker wrote while the number is not valid yet.
        assert_eq!(split("h"), Some(("".into(), "h".into())));
        assert_eq!(split("x2d"), Some(("x2".into(), "d".into())));
    }
}
