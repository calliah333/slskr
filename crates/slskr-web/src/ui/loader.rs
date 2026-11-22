//! `Loader`.

use leptos::prelude::*;

use super::classes::{Classes, KeyOrValue};

/// semantic-ui-react `Loader`: `<div class="ui {size} active ... loader">`.
/// The `text` class is added when `content` or children are given.
#[component]
pub fn Loader(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(into, optional)] active: Signal<bool>,
    #[prop(into, optional)] content: Option<Signal<String>>,
    #[prop(optional)] disabled: bool,
    #[prop(optional)] indeterminate: bool,
    /// `true` or `"centered"`.
    #[prop(into, optional)]
    inline: KeyOrValue,
    #[prop(optional)] inverted: bool,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let has_text = children.is_some() || content.is_some();
    let classes = move || {
        Classes::new("ui")
            .opt(size)
            .key(active.get(), "active")
            .key(disabled, "disabled")
            .key(indeterminate, "indeterminate")
            .key(inverted, "inverted")
            .key(has_text, "text")
            .key_or_value(inline, "inline")
            .add("loader")
            .add(&class.get())
            .build()
    };
    let inner = match children {
        Some(children) => children(),
        None => (move || content.map(|c| c.get())).into_any(),
    };
    view! { <div class=classes>{inner}</div> }
}
