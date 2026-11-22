//! `Icon` and `Icon.Group`.

use leptos::ev::MouseEvent;
use leptos::prelude::*;

use super::classes::{Classes, KeyOrValue};

/// `<i aria-hidden="true" class="{color} {name} {size} ... icon">`.
#[component]
pub fn Icon(
    /// Icon name, e.g. `"folder open"`.
    #[prop(into)]
    name: Signal<String>,
    #[prop(into, optional)] class: Signal<String>,
    #[prop(into, optional)] color: Signal<Option<String>>,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(optional)] bordered: bool,
    #[prop(optional)] circular: bool,
    #[prop(into, optional)] disabled: Signal<bool>,
    #[prop(optional)] fitted: bool,
    #[prop(optional)] inverted: bool,
    #[prop(into, optional)] link: Signal<bool>,
    #[prop(into, optional)] loading: Signal<bool>,
    /// `true` or a corner position such as `"top right"`.
    #[prop(into, optional)]
    corner: KeyOrValue,
    /// `"horizontally"` or `"vertically"`.
    #[prop(optional)]
    flipped: Option<&'static str>,
    /// `"clockwise"` or `"counterclockwise"`.
    #[prop(optional)]
    rotated: Option<&'static str>,
    /// When set, rendered as `aria-label` instead of `aria-hidden`.
    #[prop(into, optional)]
    aria_label: Option<String>,
    #[prop(into, optional)] on_click: Option<Callback<MouseEvent>>,
) -> impl IntoView {
    let classes = move || {
        Classes::new("")
            .opt(color.get().as_deref())
            .add(&name.get())
            .opt(size)
            .key(bordered, "bordered")
            .key(circular, "circular")
            .key(disabled.get(), "disabled")
            .key(fitted, "fitted")
            .key(inverted, "inverted")
            .key(link.get(), "link")
            .key(loading.get(), "loading")
            .key_or_value(corner, "corner")
            .value_key(flipped, "flipped")
            .value_key(rotated, "rotated")
            .add("icon")
            .add(&class.get())
            .build()
    };
    let aria_hidden = aria_label.is_none().then_some("true");
    let on_click_handler = move |ev: MouseEvent| {
        if disabled.get_untracked() {
            ev.prevent_default();
            return;
        }
        if let Some(cb) = on_click {
            cb.run(ev);
        }
    };
    view! { <i aria-hidden=aria_hidden aria-label=aria_label class=classes on:click=on_click_handler></i> }
}

/// `Icon.Group`: `<i class="{size} icons">`.
#[component]
pub fn IconGroup(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        Classes::new("")
            .opt(size)
            .add("icons")
            .add(&class.get())
            .build()
    };
    view! { <i class=classes>{children.map(|c| c())}</i> }
}
