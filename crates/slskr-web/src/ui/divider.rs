//! `Divider`.

use leptos::prelude::*;

use super::classes::Classes;

/// semantic-ui-react `Divider`: `<div class="ui ... divider">`.
#[component]
pub fn Divider(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] clearing: bool,
    #[prop(optional)] fitted: bool,
    #[prop(optional)] hidden: bool,
    #[prop(optional)] horizontal: bool,
    #[prop(optional)] inverted: bool,
    #[prop(optional)] section: bool,
    #[prop(optional)] vertical: bool,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        Classes::new("ui")
            .key(clearing, "clearing")
            .key(fitted, "fitted")
            .key(hidden, "hidden")
            .key(horizontal, "horizontal")
            .key(inverted, "inverted")
            .key(section, "section")
            .key(vertical, "vertical")
            .add("divider")
            .add(&class.get())
            .build()
    };
    view! { <div class=classes>{children.map(|c| c())}</div> }
}
