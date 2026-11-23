//! `Brand`: the logo mark, app name and tagline at the top left of every screen.

use leptos::prelude::*;

use super::icon::Icon;

/// Headphones mark + bold "slskr" + muted "Soulseek client". With `href` the brand is a link.
#[component]
pub fn Brand(#[prop(into, optional)] href: Option<String>) -> impl IntoView {
    let inner = view! {
        <span class="brand__logo" aria-hidden="true"><Icon name="headphones"/></span>
        <span>
            <span class="brand__name">"slskr"</span>
            <span class="brand__tagline">"Soulseek client"</span>
        </span>
    };
    match href {
        Some(href) => view! { <a class="brand" href=href>{inner}</a> }.into_any(),
        None => view! { <div class="brand">{inner}</div> }.into_any(),
    }
}
