//! Form and status controls.

use leptos::ev::Event;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::classes::Classes;

/// Semantic UI input backed by a native browser `<input>`.
#[component]
pub fn Input(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(into, optional)] value: Signal<String>,
    #[prop(into, optional)] placeholder: Option<String>,
    #[prop(optional)] input_type: Option<&'static str>,
    #[prop(optional)] name: Option<&'static str>,
    #[prop(optional)] id: Option<&'static str>,
    #[prop(optional)] disabled: bool,
    #[prop(optional)] readonly: bool,
    #[prop(optional)] required: bool,
    #[prop(optional)] autofocus: bool,
    #[prop(into, optional)] on_input: Option<Callback<String>>,
) -> impl IntoView {
    let classes = move || Classes::new("ui").add("input").add(&class.get()).build();
    let handle_input = move |ev: Event| {
        let target = ev.target().expect("input event target");
        let value = target.unchecked_into::<web_sys::HtmlInputElement>().value();
        if let Some(callback) = on_input {
            callback.run(value);
        }
    };
    view! {
        <div class=classes>
            <input
                type=input_type.unwrap_or("text")
                name=name
                id=id
                value=move || value.get()
                placeholder=placeholder
                disabled=disabled
                readonly=readonly
                required=required
                autofocus=autofocus
                on:input=handle_input
            />
        </div>
    }
}

/// Native multiline text input with Semantic UI form styling.
#[component]
pub fn TextArea(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(into, optional)] value: Signal<String>,
    #[prop(into, optional)] placeholder: Option<String>,
    #[prop(optional)] name: Option<&'static str>,
    #[prop(optional)] id: Option<&'static str>,
    #[prop(optional)] rows: Option<u32>,
    #[prop(optional)] disabled: bool,
    #[prop(optional)] readonly: bool,
    #[prop(optional)] required: bool,
    #[prop(into, optional)] on_input: Option<Callback<String>>,
) -> impl IntoView {
    let classes = move || Classes::new("ui").add(&class.get()).build();
    let handle_input = move |ev: Event| {
        let target = ev.target().expect("textarea event target");
        let value = target
            .unchecked_into::<web_sys::HtmlTextAreaElement>()
            .value();
        if let Some(callback) = on_input {
            callback.run(value);
        }
    };
    view! {
        <textarea
            class=classes
            name=name
            id=id
            rows=rows.unwrap_or(3)
            prop:value=move || value.get()
            placeholder=placeholder
            disabled=disabled
            readonly=readonly
            required=required
            on:input=handle_input
        />
    }
}

/// Accessible native checkbox, optionally styled as a Semantic UI toggle.
#[component]
pub fn Checkbox(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(into, optional)] checked: Signal<bool>,
    #[prop(into, optional)] label: Option<String>,
    #[prop(optional)] name: Option<&'static str>,
    #[prop(optional)] id: Option<&'static str>,
    #[prop(optional)] disabled: bool,
    #[prop(optional)] toggle: bool,
    #[prop(into, optional)] on_change: Option<Callback<bool>>,
) -> impl IntoView {
    let classes = move || {
        Classes::new("ui")
            .key(toggle, "toggle")
            .add("checkbox")
            .add(&class.get())
            .build()
    };
    let handle_change = move |ev: Event| {
        let input = ev
            .target()
            .expect("checkbox event target")
            .unchecked_into::<web_sys::HtmlInputElement>();
        if let Some(callback) = on_change {
            callback.run(input.checked());
        }
    };
    view! {
        <div class=classes>
            <input
                type="checkbox"
                name=name
                id=id
                prop:checked=move || checked.get()
                disabled=disabled
                on:change=handle_change
            />
            <label for=id>{label}</label>
        </div>
    }
}

/// Semantic UI progress bar; `value=None` renders an indeterminate bar.
#[component]
pub fn Progress(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(into, optional)] value: Option<Signal<f64>>,
    #[prop(optional)] total: f64,
    #[prop(optional)] indicating: bool,
    #[prop(optional)] success: bool,
    #[prop(optional)] warning: bool,
    #[prop(optional)] error: bool,
    #[prop(into, optional)] label: Option<String>,
) -> impl IntoView {
    let total = if total > 0.0 { total } else { 100.0 };
    let classes = move || {
        Classes::new("ui")
            .key(indicating, "indicating")
            .key(success, "success")
            .key(warning, "warning")
            .key(error, "error")
            .add(&class.get())
            .add("progress")
            .build()
    };
    let percent = move || value.map(|v| (v.get() / total * 100.0).clamp(0.0, 100.0));
    let width = move || percent().map(|p| format!("{p}%"));
    let aria_value = move || percent().map(|p| p.to_string());
    let aria_label = label.clone();
    let progress_label = label.clone();
    view! {
        <div
            class=classes
            role="progressbar"
            aria-valuemin="0"
            aria-valuemax=total.to_string()
            aria-valuenow=move || value.map(|v| v.get().to_string())
            aria-label=aria_label
        >
            <div class="bar" style:width=width>
                <div class="progress">{move || progress_label.clone().or_else(|| aria_value().map(|p| format!("{p}%")))}</div>
            </div>
        </div>
    }
}

/// Tooltip using the browser's native, keyboard-compatible `title` behavior.
#[component]
pub fn Tooltip(
    #[prop(into)] text: String,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    view! {
        <span title=text>
            {children.map(|children| children())}
        </span>
    }
}

/// Accessible live-region status notification.
#[component]
pub fn Toast(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(into, optional)] content: Option<Signal<String>>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let inner = match children {
        Some(children) => children().into_any(),
        None => view! { {move || content.map(|content| content.get())} }.into_any(),
    };
    let classes = move || Classes::new("ui").add("message").add(&class.get()).build();
    view! {
        <div class=classes role="status" aria-live="polite" aria-atomic="true">
            {inner}
        </div>
    }
}
