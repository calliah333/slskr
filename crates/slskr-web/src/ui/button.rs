//! `Button`: `<button class="btn btn--{variant} btn--{size}">` (or `<a>` with `href`).

use leptos::ev::MouseEvent;
use leptos::prelude::*;

use super::icon::Icon;
use super::join_classes;

/// A button.
///
/// - `variant`: `"outline"` (default: card background with a border, like teacup's
///   Logout), `"primary"` (solid blue), `"secondary"` (muted fill), `"ghost"`,
///   `"destructive"` (solid red), `"danger-outline"` (red text), `"link"` or `"action"`
///   (quiet icon button for table rows).
/// - `size`: `"xs"`, `"sm"`, `"lg"`.
/// - `icon`: a leading [`Icon`] name. Without children (and `text`) the button is a
///   square icon button, and `label` is required: it becomes `aria-label` and `title`.
/// - `loading`: shows a spinner and disables the button.
/// - `pressed`: renders `aria-pressed` (toggle buttons).
/// - `href`: renders an `<a class="btn">` instead (navigation that looks like a button).
#[component]
pub fn Button(
    #[prop(optional)] variant: Option<&'static str>,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(into, optional)] icon: Option<Signal<String>>,
    /// Text content when there are no children.
    #[prop(into, optional)]
    text: Option<Signal<String>>,
    /// Accessible name and tooltip (required for icon-only buttons).
    #[prop(into, optional)]
    label: Option<Signal<String>>,
    #[prop(into, optional)] disabled: Signal<bool>,
    #[prop(into, optional)] loading: Signal<bool>,
    #[prop(into, optional)] pressed: Option<Signal<bool>>,
    /// Full width.
    #[prop(optional)]
    block: bool,
    /// The `type` attribute; default `"button"`.
    #[prop(optional)]
    button_type: Option<&'static str>,
    #[prop(into, optional)] href: Option<String>,
    #[prop(into, optional)] class: Signal<String>,
    #[prop(into, optional)] on_click: Option<Callback<MouseEvent>>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let icon_only = children.is_none() && text.is_none();
    let classes = move || {
        let variant_class = variant.map(|v| format!("btn--{v}"));
        let size_class = size.map(|s| format!("btn--{s}"));
        join_classes(&[
            "btn",
            variant_class.as_deref().unwrap_or(""),
            size_class.as_deref().unwrap_or(""),
            if icon_only && variant != Some("link") {
                "btn--icon"
            } else {
                ""
            },
            if block { "btn--block" } else { "" },
            &class.get(),
        ])
    };
    let is_disabled = move || disabled.get() || loading.get();
    let handle_click = move |ev: MouseEvent| {
        if is_disabled() {
            ev.prevent_default();
            return;
        }
        if let Some(cb) = on_click {
            cb.run(ev);
        }
    };
    let leading = move || {
        if loading.get() {
            view! { <span class="spinner" aria-hidden="true"></span> }.into_any()
        } else {
            icon.map(|name| view! { <Icon name=name/> }).into_any()
        }
    };
    let inner = match children {
        Some(children) => children(),
        None => (move || text.map(|t| view! { <span>{t.get()}</span> })).into_any(),
    };
    let aria_label = move || label.map(|l| l.get());
    let aria_pressed = move || pressed.map(|p| p.get().to_string());
    let busy = move || loading.get().then_some("true");
    match href {
        Some(href) => view! {
            <a
                class=classes
                href=href
                aria-label=aria_label
                title=aria_label
                aria-disabled=move || is_disabled().then_some("true")
                on:click=handle_click
            >
                {leading}
                {inner}
            </a>
        }
        .into_any(),
        None => view! {
            <button
                type=button_type.unwrap_or("button")
                class=classes
                aria-label=aria_label
                title=aria_label
                aria-pressed=aria_pressed
                aria-busy=busy
                disabled=is_disabled
                on:click=handle_click
            >
                {leading}
                {inner}
            </button>
        }
        .into_any(),
    }
}
