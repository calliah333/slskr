//! `Label`, `Label.Detail`, `Label.Group`.

use leptos::ev::MouseEvent;
use leptos::html::custom;
use leptos::prelude::*;

use super::classes::{Classes, KeyOrValue};
use super::icon::Icon;

/// semantic-ui-react `Label`.
///
/// `pointing`: `true`, `"left"`, `"right"`, `"above"`, `"below"`. `as_`: element
/// (default `"div"`). `on_remove` renders the trailing `delete` icon.
#[component]
pub fn Label(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] as_: Option<&'static str>,
    #[prop(into, optional)] active: Signal<bool>,
    #[prop(optional)] attached: Option<&'static str>,
    #[prop(optional)] basic: bool,
    #[prop(optional)] circular: bool,
    #[prop(into, optional)] color: Signal<Option<String>>,
    #[prop(into, optional)] content: Option<Signal<String>>,
    #[prop(into, optional)] corner: KeyOrValue,
    #[prop(into, optional)] detail: Option<String>,
    #[prop(optional)] empty: bool,
    #[prop(optional)] floating: bool,
    #[prop(optional)] horizontal: bool,
    #[prop(optional)] icon: Option<&'static str>,
    #[prop(optional)] image: bool,
    #[prop(into, optional)] pointing: KeyOrValue,
    #[prop(optional)] prompt: bool,
    #[prop(into, optional)] ribbon: KeyOrValue,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(optional)] tag: bool,
    #[prop(into, optional)] on_click: Option<Callback<MouseEvent>>,
    #[prop(into, optional)] on_remove: Option<Callback<MouseEvent>>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let pointing_class = match pointing {
        KeyOrValue::On => Some("pointing".to_string()),
        KeyOrValue::Value(p @ ("left" | "right")) => Some(format!("{p} pointing")),
        KeyOrValue::Value(p @ ("above" | "below")) => Some(format!("pointing {p}")),
        _ => None,
    };
    let classes = move || {
        Classes::new("ui")
            .opt(color.get().as_deref())
            .opt(pointing_class.as_deref())
            .opt(size)
            .key(active.get(), "active")
            .key(basic, "basic")
            .key(circular, "circular")
            .key(empty, "empty")
            .key(floating, "floating")
            .key(horizontal, "horizontal")
            .key(image, "image")
            .key(prompt, "prompt")
            .key(tag, "tag")
            .key_or_value(corner, "corner")
            .key_or_value(ribbon, "ribbon")
            .value_key(attached, "attached")
            .add("label")
            .add(&class.get())
            .build()
    };
    let inner = match children {
        Some(children) => children(),
        None => {
            let remove = on_remove.map(|cb| {
                view! { <Icon name="delete" on_click=move |ev: MouseEvent| cb.run(ev) /> }
            });
            view! {
                {icon.map(|name| view! { <Icon name=name /> })}
                {move || content.map(|c| c.get())}
                {detail.map(|d| view! { <div class="detail">{d}</div> })}
                {remove}
            }
            .into_any()
        }
    };
    custom(as_.unwrap_or("div"))
        .class(classes)
        .on(leptos::ev::click, move |ev: MouseEvent| {
            if let Some(cb) = on_click {
                cb.run(ev);
            }
        })
        .child(inner)
}

/// `Label.Detail`: `<div class="detail">`.
#[component]
pub fn LabelDetail(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || Classes::new("detail").add(&class.get()).build();
    view! { <div class=classes>{children.map(|c| c())}</div> }
}

/// `Label.Group`: `<div class="ui ... labels">`.
#[component]
pub fn LabelGroup(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] circular: bool,
    #[prop(optional)] color: Option<&'static str>,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(optional)] tag: bool,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        Classes::new("ui")
            .opt(color)
            .opt(size)
            .key(circular, "circular")
            .key(tag, "tag")
            .add("labels")
            .add(&class.get())
            .build()
    };
    view! { <div class=classes>{children.map(|c| c())}</div> }
}
