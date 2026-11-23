//! Layout containers: `Card`, `PageHeader`, `EmptyState`, `Stat`.

use leptos::prelude::*;

use super::icon::Icon;
use super::join_classes;

/// A white rounded card: `<section class="card">` with an optional header (title,
/// description, actions on the right), the children in `card__content` and an optional
/// `card__footer`.
///
/// `flush` drops the content padding (a table or list that reaches the card edges);
/// `compact` uses smaller padding. `title_id` sets the heading id (the section is then
/// labelled by it).
#[component]
pub fn Card(
    #[prop(into, optional)] title: Option<String>,
    #[prop(into, optional)] description: Option<String>,
    /// Buttons at the right of the header.
    #[prop(optional)]
    actions: Option<ViewFn>,
    #[prop(optional)] footer: Option<ViewFn>,
    #[prop(optional)] flush: bool,
    #[prop(optional)] compact: bool,
    /// Heading element for the title (default `h2`): `"h2"`, `"h3"`.
    #[prop(optional)]
    heading: Option<&'static str>,
    #[prop(into, optional)] title_id: Option<String>,
    #[prop(into, optional)] id: Option<String>,
    #[prop(into, optional)] class: Signal<String>,
    children: Children,
) -> impl IntoView {
    let classes = move || {
        join_classes(&[
            "card",
            if flush { "card--flush" } else { "" },
            if compact { "card--compact" } else { "" },
            &class.get(),
        ])
    };
    let has_header = title.is_some() || actions.is_some();
    let labelled_by = title.as_ref().and(title_id.clone());
    let header = has_header.then(|| {
        let title = title.map(|title| {
            let id = title_id.clone();
            match heading.unwrap_or("h2") {
                "h3" => view! { <h3 class="card__title" id=id>{title}</h3> }.into_any(),
                _ => view! { <h2 class="card__title" id=id>{title}</h2> }.into_any(),
            }
        });
        view! {
            <div class="card__header">
                <div>
                    {title}
                    {description.map(|d| view! { <p class="card__description">{d}</p> })}
                </div>
                {actions.map(|a| view! { <div class="card__actions">{a.run()}</div> })}
            </div>
        }
    });
    view! {
        <section class=classes id=id aria-labelledby=labelled_by>
            {header}
            <div class="card__content">{children()}</div>
            {footer.map(|f| view! { <div class="card__footer">{f.run()}</div> })}
        </section>
    }
}

/// The heading row of a page: title and description on the left, actions on the right.
#[component]
pub fn PageHeader(
    #[prop(into)] title: Signal<String>,
    #[prop(into, optional)] description: Option<Signal<String>>,
    #[prop(optional)] actions: Option<ViewFn>,
) -> impl IntoView {
    view! {
        <div class="page-header">
            <div>
                <h1 class="page-header__title">{move || title.get()}</h1>
                {description.map(|d| view! { <p class="page-header__description">{move || d.get()}</p> })}
            </div>
            {actions.map(|a| view! { <div class="page-header__actions">{a.run()}</div> })}
        </div>
    }
}

/// A centered placeholder for "nothing here yet": icon in a tinted circle, title,
/// description and optional actions (children).
#[component]
pub fn EmptyState(
    #[prop(optional)] icon: Option<&'static str>,
    #[prop(into)] title: Signal<String>,
    #[prop(into, optional)] description: Option<Signal<String>>,
    /// Dashed border (drop-zone look).
    #[prop(optional)]
    dashed: bool,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let class = if dashed {
        "empty-state empty-state--dashed"
    } else {
        "empty-state"
    };
    view! {
        <div class=class>
            {icon.map(|name| view! { <div class="empty-state__icon"><Icon name=name/></div> })}
            <p class="empty-state__title">{move || title.get()}</p>
            {description.map(|d| view! { <p class="empty-state__description">{move || d.get()}</p> })}
            {children.map(|c| view! { <div class="empty-state__actions">{c()}</div> })}
        </div>
    }
}

/// A stat tile: small muted label above a bold value, optional hint below. Children (e.g.
/// a `Progress`) go after the value. Put several in `<div class="stats">`.
#[component]
pub fn Stat(
    #[prop(into)] label: String,
    #[prop(into)] value: Signal<String>,
    #[prop(into, optional)] hint: Option<Signal<String>>,
    /// Larger value text (dashboard headline numbers).
    #[prop(optional)]
    large: bool,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let value_class = if large {
        "stat__value stat__value--lg"
    } else {
        "stat__value"
    };
    view! {
        <div class="stat">
            <div class="stat__label">{label}</div>
            <div class=value_class>{move || value.get()}</div>
            {hint.map(|h| view! { <div class="stat__hint">{move || h.get()}</div> })}
            {children.map(|c| c())}
        </div>
    }
}
