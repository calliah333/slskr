//! `Segment`, `Segment.Group`, `Segment.Inline`.

use leptos::prelude::*;

use super::classes::{Classes, KeyOrValue};

/// Builds the `Segment` class string (shared with `Tab.Pane` and `Sidebar.Pushable`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn segment_classes(
    color: Option<&str>,
    size: Option<&str>,
    flags: &[(bool, &str)],
    attached: KeyOrValue,
    padded: KeyOrValue,
    text_align: Option<&str>,
    floated: Option<&str>,
    class: &str,
) -> String {
    let mut c = Classes::new("ui").opt(color).opt(size);
    for (on, key) in flags {
        c = c.key(*on, key);
    }
    c.key_or_value(attached, "attached")
        .key_or_value(padded, "padded")
        .text_align(text_align)
        .value_key(floated, "floated")
        .add("segment")
        .add(class)
        .build()
}

/// semantic-ui-react `Segment`: `<div class="ui ... segment">`.
#[component]
pub fn Segment(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(into, optional)] attached: KeyOrValue,
    #[prop(optional)] basic: bool,
    #[prop(optional)] circular: bool,
    #[prop(optional)] clearing: bool,
    #[prop(optional)] color: Option<&'static str>,
    #[prop(optional)] compact: bool,
    #[prop(into, optional)] disabled: Signal<bool>,
    #[prop(optional)] floated: Option<&'static str>,
    #[prop(optional)] inverted: bool,
    #[prop(into, optional)] loading: Signal<bool>,
    #[prop(into, optional)] padded: KeyOrValue,
    #[prop(optional)] placeholder: bool,
    #[prop(optional)] piled: bool,
    #[prop(optional)] raised: bool,
    #[prop(optional)] secondary: bool,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(optional)] stacked: bool,
    #[prop(optional)] tertiary: bool,
    #[prop(optional)] text_align: Option<&'static str>,
    #[prop(optional)] vertical: bool,
    #[prop(optional)] node_ref: NodeRef<leptos::html::Div>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        segment_classes(
            color,
            size,
            &[
                (basic, "basic"),
                (circular, "circular"),
                (clearing, "clearing"),
                (compact, "compact"),
                (disabled.get(), "disabled"),
                (inverted, "inverted"),
                (loading.get(), "loading"),
                (placeholder, "placeholder"),
                (piled, "piled"),
                (raised, "raised"),
                (secondary, "secondary"),
                (stacked, "stacked"),
                (tertiary, "tertiary"),
                (vertical, "vertical"),
            ],
            attached,
            padded,
            text_align,
            floated,
            &class.get(),
        )
    };
    view! { <div class=classes node_ref=node_ref>{children.map(|c| c())}</div> }
}

/// `Segment.Group`: `<div class="ui ... segments">`.
#[component]
pub fn SegmentGroup(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] compact: bool,
    #[prop(optional)] horizontal: bool,
    #[prop(optional)] piled: bool,
    #[prop(optional)] raised: bool,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(optional)] stacked: bool,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        Classes::new("ui")
            .opt(size)
            .key(compact, "compact")
            .key(horizontal, "horizontal")
            .key(piled, "piled")
            .key(raised, "raised")
            .key(stacked, "stacked")
            .add("segments")
            .add(&class.get())
            .build()
    };
    view! { <div class=classes>{children.map(|c| c())}</div> }
}

/// `Segment.Inline`: `<div class="inline">`.
#[component]
pub fn SegmentInline(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || Classes::new("inline").add(&class.get()).build();
    view! { <div class=classes>{children.map(|c| c())}</div> }
}
