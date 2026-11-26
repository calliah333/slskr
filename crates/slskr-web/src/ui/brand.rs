//! `Brand`: the logo mark, app name and running version at the top left of every screen.

use leptos::prelude::*;

use super::icon::Icon;

/// Headphones mark + bold "slskr", and the muted running `version` below it when given
/// (`title` is the full build). With `href` the brand is a link.
#[component]
pub fn Brand(
    #[prop(into, optional)] href: Option<String>,
    #[prop(into, optional)] version: Option<Signal<String>>,
    #[prop(into, optional)] title: Option<Signal<String>>,
) -> impl IntoView {
    let inner = view! {
        <span class="brand__logo" aria-hidden="true"><Icon name="headphones"/></span>
        <span>
            <span class="brand__name">"slskr"</span>
            {version.map(|version| view! {
                <span class="brand__version mono" title=move || title.map(|title| title.get())>
                    {move || version.get()}
                </span>
            })}
        </span>
    };
    match href {
        Some(href) => view! { <a class="brand" href=href>{inner}</a> }.into_any(),
        None => view! { <div class="brand">{inner}</div> }.into_any(),
    }
}
