//! `Image`, `Image.Group`.

use leptos::prelude::*;

use super::classes::{Classes, KeyOrValue};

/// Builds the `Image` class string.
#[allow(clippy::too_many_arguments)]
pub(crate) fn image_classes(
    ui: bool,
    size: Option<&str>,
    flags: &[(bool, &str)],
    spaced: KeyOrValue,
    floated: Option<&str>,
    vertical_align: Option<&str>,
    class: &str,
) -> String {
    let mut c = Classes::new("").key(ui, "ui").opt(size);
    for (on, key) in flags {
        c = c.key(*on, key);
    }
    c.key_or_value(spaced, "spaced")
        .value_key(floated, "floated")
        .vertical_align(vertical_align)
        .add("image")
        .add(class)
        .build()
}

/// semantic-ui-react `Image`: `<img class="ui ... image">`, or a
/// `<div class="... image"><img/></div>` when `wrapped` / children are given.
#[component]
pub fn Image(
    #[prop(into, optional)] src: Option<Signal<String>>,
    #[prop(into, optional)] alt: Option<String>,
    #[prop(into, optional)] class: Signal<String>,
    /// Adds the `ui` class (default `true`).
    #[prop(default = true)]
    ui: bool,
    #[prop(optional)] avatar: bool,
    #[prop(optional)] bordered: bool,
    #[prop(optional)] centered: bool,
    #[prop(optional)] circular: bool,
    #[prop(optional)] disabled: bool,
    #[prop(optional)] fluid: bool,
    #[prop(optional)] hidden: bool,
    #[prop(optional)] inline: bool,
    #[prop(optional)] rounded: bool,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(into, optional)] spaced: KeyOrValue,
    #[prop(optional)] floated: Option<&'static str>,
    #[prop(optional)] vertical_align: Option<&'static str>,
    #[prop(optional)] wrapped: bool,
    #[prop(into, optional)] href: Option<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        image_classes(
            ui,
            size,
            &[
                (avatar, "avatar"),
                (bordered, "bordered"),
                (circular, "circular"),
                (centered, "centered"),
                (disabled, "disabled"),
                (fluid, "fluid"),
                (hidden, "hidden"),
                (inline, "inline"),
                (rounded, "rounded"),
            ],
            spaced,
            floated,
            vertical_align,
            &class.get(),
        )
    };
    let src = move || src.map(|s| s.get());
    if let Some(children) = children {
        return view! { <div class=classes>{children()}</div> }.into_any();
    }
    if wrapped {
        return match href {
            Some(href) => {
                view! { <a class=classes href=href><img src=src alt=alt /></a> }.into_any()
            }
            None => view! { <div class=classes><img src=src alt=alt /></div> }.into_any(),
        };
    }
    view! { <img class=classes src=src alt=alt /> }.into_any()
}

/// `Image.Group`: `<div class="ui {size} images">`.
#[component]
pub fn ImageGroup(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        Classes::new("ui")
            .opt(size)
            .add("images")
            .add(&class.get())
            .build()
    };
    view! { <div class=classes>{children.map(|c| c())}</div> }
}
