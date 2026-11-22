//! `Header`, `Header.Content`, `Header.Subheader`.

use leptos::html::custom;
use leptos::prelude::*;

use super::classes::{Classes, KeyOrValue};
use super::icon::Icon;

/// semantic-ui-react `Header`.
///
/// `as_` selects the element (`"h1"`..`"h6"`, default `"div"`). `icon`: `true`
/// adds the `icon` class (icon passed as child); a name renders the icon
/// shorthand followed by `content`/`subheader` wrapped in `Header.Content`.
#[component]
pub fn Header(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] as_: Option<&'static str>,
    #[prop(into, optional)] attached: KeyOrValue,
    #[prop(optional)] block: bool,
    #[prop(optional)] color: Option<&'static str>,
    #[prop(into, optional)] content: Option<Signal<String>>,
    #[prop(optional)] disabled: bool,
    #[prop(optional)] dividing: bool,
    #[prop(optional)] floated: Option<&'static str>,
    #[prop(into, optional)] icon: KeyOrValue,
    #[prop(optional)] image: bool,
    #[prop(optional)] inverted: bool,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(optional)] sub: bool,
    #[prop(into, optional)] subheader: Option<String>,
    #[prop(optional)] text_align: Option<&'static str>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        Classes::new("ui")
            .opt(color)
            .opt(size)
            .key(block, "block")
            .key(disabled, "disabled")
            .key(dividing, "dividing")
            .value_key(floated, "floated")
            .key(icon == KeyOrValue::On, "icon")
            .key(image, "image")
            .key(inverted, "inverted")
            .key(sub, "sub")
            .key_or_value(attached, "attached")
            .text_align(text_align)
            .add("header")
            .add(&class.get())
            .build()
    };
    let inner = match children {
        Some(children) => children(),
        None => {
            let content_view = move || content.map(|c| c.get());
            let subheader_view = subheader.map(|s| view! { <div class="sub header">{s}</div> });
            match icon.value() {
                Some(name) => {
                    let has_inner = content.is_some() || subheader_view.is_some();
                    view! {
                        <Icon name=name />
                        {has_inner.then(|| view! { <div class="content">{content_view}{subheader_view}</div> })}
                    }
                    .into_any()
                }
                None => view! { {content_view}{subheader_view} }.into_any(),
            }
        }
    };
    custom(as_.unwrap_or("div")).class(classes).child(inner)
}

/// `Header.Content`: `<div class="content">`.
#[component]
pub fn HeaderContent(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || Classes::new("content").add(&class.get()).build();
    view! { <div class=classes>{children.map(|c| c())}</div> }
}

/// `Header.Subheader`: `<div class="sub header">`.
#[component]
pub fn HeaderSubheader(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || Classes::new("sub header").add(&class.get()).build();
    view! { <div class=classes>{children.map(|c| c())}</div> }
}
