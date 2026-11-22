//! `Flag`.

use leptos::prelude::*;

use super::classes::Classes;

/// semantic-ui-react `Flag`: `<i class="{name} flag">` (e.g. `name="us"`).
#[component]
pub fn Flag(
    #[prop(into)] name: Signal<String>,
    #[prop(into, optional)] class: Signal<String>,
) -> impl IntoView {
    let classes = move || {
        Classes::new(&name.get())
            .add("flag")
            .add(&class.get())
            .build()
    };
    view! { <i class=classes></i> }
}
