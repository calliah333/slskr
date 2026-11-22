//! `Button`, `Button.Group`, `Button.Or`, `Button.Content`.

use leptos::ev::MouseEvent;
use leptos::prelude::*;

use super::classes::{Classes, KeyOrValue, Width};
use super::icon::Icon;

/// semantic-ui-react `Button`.
///
/// `icon`: `true` adds the `icon` class (icon given as a child), a name renders
/// the icon shorthand. `href` renders an `<a role="button">`. `label` (with
/// `label_position`) renders the labeled-button wrapper; pass the label element
/// itself (semantic-ui-react defaults it to `<Label basic pointing=...>`).
#[component]
pub fn Button(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(into, optional)] active: Signal<bool>,
    #[prop(into, optional)] animated: KeyOrValue,
    #[prop(into, optional)] attached: KeyOrValue,
    #[prop(optional)] basic: bool,
    #[prop(optional)] circular: bool,
    #[prop(into, optional)] color: Signal<Option<String>>,
    #[prop(optional)] compact: bool,
    /// Text content used when there are no children.
    #[prop(into, optional)]
    content: Option<Signal<String>>,
    #[prop(into, optional)] disabled: Signal<bool>,
    #[prop(optional)] floated: Option<&'static str>,
    #[prop(optional)] fluid: bool,
    #[prop(into, optional)] icon: KeyOrValue,
    #[prop(optional)] inverted: bool,
    #[prop(into, optional)] label: Option<ViewFn>,
    /// `"left"` or `"right"`.
    #[prop(optional)]
    label_position: Option<&'static str>,
    #[prop(into, optional)] loading: Signal<bool>,
    #[prop(into, optional)] negative: Signal<bool>,
    #[prop(into, optional)] positive: Signal<bool>,
    #[prop(into, optional)] primary: Signal<bool>,
    #[prop(optional)] secondary: bool,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(optional)] toggle: bool,
    /// The `type` attribute (`"button"`, `"submit"`).
    #[prop(optional)]
    button_type: Option<&'static str>,
    #[prop(into, optional)] href: Option<String>,
    #[prop(optional)] tab_index: Option<i32>,
    #[prop(into, optional)] on_click: Option<Callback<MouseEvent>>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let has_children = children.is_some();
    let has_content = content.is_some();
    let has_icon_class = match icon {
        KeyOrValue::On => true,
        KeyOrValue::Value(_) => label_position.is_some() || (!has_children && !has_content),
        KeyOrValue::Off => false,
    };
    let base = move |c: Classes| {
        c.opt(color.get().as_deref())
            .opt(size)
            .key(active.get(), "active")
            .key(basic, "basic")
            .key(circular, "circular")
            .key(compact, "compact")
            .key(fluid, "fluid")
            .key(has_icon_class, "icon")
            .key(inverted, "inverted")
            .key(loading.get(), "loading")
            .key(negative.get(), "negative")
            .key(positive.get(), "positive")
            .key(primary.get(), "primary")
            .key(secondary, "secondary")
            .key(toggle, "toggle")
            .key_or_value(animated, "animated")
            .key_or_value(attached, "attached")
    };
    let labeled = match label_position {
        Some(pos) => KeyOrValue::Value(pos),
        None => KeyOrValue::from(label.is_some()),
    };
    let wrapper = move |c: Classes| {
        c.key(disabled.get(), "disabled")
            .value_key(floated, "floated")
    };
    let aria_pressed = move || toggle.then(|| active.get().to_string());
    let handle_click = move |ev: MouseEvent| {
        if disabled.get_untracked() {
            ev.prevent_default();
            return;
        }
        if let Some(cb) = on_click {
            cb.run(ev);
        }
    };
    let icon_view = icon.value().map(|name| view! { <Icon name=name /> });

    if let Some(label) = label {
        let container = move || {
            wrapper(
                Classes::new("ui")
                    .key_or_value(labeled, "labeled")
                    .add("button")
                    .add(&class.get()),
            )
            .build()
        };
        let button_classes = move || {
            base(Classes::new("ui"))
                .add("button")
                .add(&class.get())
                .build()
        };
        let tab = move || tab_index.unwrap_or(if disabled.get() { -1 } else { 0 });
        let left = (label_position == Some("left")).then(|| label.run());
        let right = (label_position != Some("left")).then(|| label.run());
        return view! {
            <div class=container on:click=handle_click>
                {left}
                <button
                    class=button_classes
                    aria-pressed=aria_pressed
                    disabled=move || disabled.get()
                    type=button_type
                    tabindex=tab
                >
                    {icon_view}
                    " "
                    {move || content.map(|c| c.get())}
                </button>
                {right}
            </div>
        }
        .into_any();
    }

    let classes = move || {
        wrapper(base(Classes::new("ui")))
            .key_or_value(labeled, "labeled")
            .add("button")
            .add(&class.get())
            .build()
    };
    let inner = match children {
        Some(children) => children(),
        None => view! { {icon_view} {move || content.map(|c| c.get())} }.into_any(),
    };
    match href {
        Some(href) => {
            let tab = move || tab_index.or_else(|| disabled.get().then_some(-1));
            view! {
                <a
                    class=classes
                    href=href
                    role="button"
                    aria-pressed=aria_pressed
                    type=button_type
                    tabindex=tab
                    on:click=handle_click
                >
                    {inner}
                </a>
            }
            .into_any()
        }
        None => {
            let tab = move || tab_index.or_else(|| disabled.get().then_some(-1));
            view! {
                <button
                    class=classes
                    aria-pressed=aria_pressed
                    disabled=move || disabled.get()
                    type=button_type
                    tabindex=tab
                    on:click=handle_click
                >
                    {inner}
                </button>
            }
            .into_any()
        }
    }
}

/// `Button.Group`: `<div class="ui ... buttons">`.
#[component]
pub fn ButtonGroup(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(into, optional)] attached: KeyOrValue,
    #[prop(optional)] basic: bool,
    #[prop(into, optional)] color: Signal<Option<String>>,
    #[prop(optional)] compact: bool,
    #[prop(optional)] floated: Option<&'static str>,
    #[prop(optional)] fluid: bool,
    #[prop(optional)] icon: bool,
    #[prop(optional)] inverted: bool,
    #[prop(optional)] labeled: bool,
    #[prop(optional)] negative: bool,
    #[prop(optional)] positive: bool,
    #[prop(optional)] primary: bool,
    #[prop(optional)] secondary: bool,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(optional)] toggle: bool,
    #[prop(optional)] vertical: bool,
    #[prop(into, optional)] widths: Option<Width>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        Classes::new("ui")
            .opt(color.get().as_deref())
            .opt(size)
            .key(basic, "basic")
            .key(compact, "compact")
            .key(fluid, "fluid")
            .key(icon, "icon")
            .key(inverted, "inverted")
            .key(labeled, "labeled")
            .key(negative, "negative")
            .key(positive, "positive")
            .key(primary, "primary")
            .key(secondary, "secondary")
            .key(toggle, "toggle")
            .key(vertical, "vertical")
            .key_or_value(attached, "attached")
            .value_key(floated, "floated")
            .width(widths, "", false)
            .add("buttons")
            .add(&class.get())
            .build()
    };
    view! { <div class=classes>{children.map(|c| c())}</div> }
}

/// `Button.Or`: `<div class="or" data-text=...>`.
#[component]
pub fn ButtonOr(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(into, optional)] text: Option<String>,
) -> impl IntoView {
    let classes = move || Classes::new("or").add(&class.get()).build();
    view! { <div class=classes data-text=text></div> }
}

/// `Button.Content`: `<div class="visible content">` (animated buttons).
#[component]
pub fn ButtonContent(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] hidden: bool,
    #[prop(optional)] visible: bool,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        Classes::new("")
            .key(hidden, "hidden")
            .key(visible, "visible")
            .add("content")
            .add(&class.get())
            .build()
    };
    view! { <div class=classes>{children.map(|c| c())}</div> }
}
