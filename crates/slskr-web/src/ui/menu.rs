//! `Menu`, `Menu.Item`, `Menu.Menu`, `Menu.Header`.

use leptos::ev::MouseEvent;
use leptos::html::custom;
use leptos::prelude::*;

use super::classes::{Classes, KeyOrValue, Width};
use super::icon::Icon;

/// Builds the `Menu` class string (shared with `Sidebar as={Menu}`, `Tab`, `Pagination`).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MenuClasses {
    pub color: Option<&'static str>,
    pub size: Option<&'static str>,
    pub borderless: bool,
    pub compact: bool,
    pub fluid: bool,
    pub inverted: bool,
    pub pagination: bool,
    pub pointing: bool,
    pub secondary: bool,
    pub stackable: bool,
    pub text: bool,
    pub vertical: bool,
    pub attached: KeyOrValue,
    pub floated: KeyOrValue,
    pub icon: KeyOrValue,
    pub tabular: KeyOrValue,
    pub fixed: Option<&'static str>,
    pub widths: Option<Width>,
}

impl MenuClasses {
    pub fn build(&self, class: &str) -> String {
        Classes::new("ui")
            .opt(self.color)
            .opt(self.size)
            .key(self.borderless, "borderless")
            .key(self.compact, "compact")
            .key(self.fluid, "fluid")
            .key(self.inverted, "inverted")
            .key(self.pagination, "pagination")
            .key(self.pointing, "pointing")
            .key(self.secondary, "secondary")
            .key(self.stackable, "stackable")
            .key(self.text, "text")
            .key(self.vertical, "vertical")
            .key_or_value(self.attached, "attached")
            .key_or_value(self.floated, "floated")
            .key_or_value(self.icon, "icon")
            .key_or_value(self.tabular, "tabular")
            .value_key(self.fixed, "fixed")
            .width(self.widths, "item", false)
            .add(class)
            .add("menu")
            .build()
    }
}

/// semantic-ui-react `Menu`: `<div class="ui ... menu">`.
#[component]
pub fn Menu(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(into, optional)] attached: KeyOrValue,
    #[prop(optional)] borderless: bool,
    #[prop(optional)] color: Option<&'static str>,
    #[prop(optional)] compact: bool,
    #[prop(optional)] fixed: Option<&'static str>,
    #[prop(into, optional)] floated: KeyOrValue,
    #[prop(optional)] fluid: bool,
    /// `true` or `"labeled"`.
    #[prop(into, optional)]
    icon: KeyOrValue,
    #[prop(optional)] inverted: bool,
    #[prop(optional)] pagination: bool,
    #[prop(optional)] pointing: bool,
    #[prop(optional)] secondary: bool,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(optional)] stackable: bool,
    #[prop(into, optional)] tabular: KeyOrValue,
    #[prop(optional)] text: bool,
    #[prop(optional)] vertical: bool,
    #[prop(into, optional)] widths: Option<Width>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let spec = MenuClasses {
        color,
        size,
        borderless,
        compact,
        fluid,
        inverted,
        pagination,
        pointing,
        secondary,
        stackable,
        text,
        vertical,
        attached,
        floated,
        icon,
        tabular,
        fixed,
        widths,
    };
    let classes = move || spec.build(&class.get());
    view! { <div class=classes>{children.map(|c| c())}</div> }
}

/// `Menu.Item`. Renders an `<a>` when `on_click` is set or `as_="a"`, otherwise a `<div>`.
///
/// Without children, renders the `icon` shorthand followed by `content`
/// (or the start-cased `name`).
#[component]
pub fn MenuItem(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] as_: Option<&'static str>,
    #[prop(into, optional)] active: Signal<bool>,
    #[prop(optional)] color: Option<&'static str>,
    #[prop(into, optional)] content: Option<Signal<String>>,
    #[prop(into, optional)] disabled: Signal<bool>,
    #[prop(into, optional)] fitted: KeyOrValue,
    #[prop(optional)] header: bool,
    /// `true` adds the `icon` class; a name renders the icon shorthand.
    #[prop(into, optional)]
    icon: KeyOrValue,
    #[prop(optional)] link: bool,
    #[prop(into, optional)] name: Option<String>,
    /// `"left"` / `"right"`.
    #[prop(optional)]
    position: Option<&'static str>,
    #[prop(into, optional)] href: Option<String>,
    #[prop(into, optional)] on_click: Option<Callback<MouseEvent>>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let icon_class = match icon {
        KeyOrValue::On => true,
        KeyOrValue::Value(_) => name.is_none() && content.is_none(),
        KeyOrValue::Off => false,
    };
    let classes = move || {
        Classes::new("")
            .opt(color)
            .opt(position)
            .key(active.get(), "active")
            .key(disabled.get(), "disabled")
            .key(icon_class, "icon")
            .key(header, "header")
            .key(link, "link")
            .key_or_value(fitted, "fitted")
            .add("item")
            .add(&class.get())
            .build()
    };
    let tag = as_.unwrap_or(if on_click.is_some() || href.is_some() {
        "a"
    } else {
        "div"
    });
    let inner = match children {
        Some(children) => children(),
        None => {
            let fallback = name.as_deref().map(start_case);
            view! {
                {icon.value().map(|n| view! { <Icon name=n /> })}
                {move || content.map(|c| c.get()).or_else(|| fallback.clone())}
            }
            .into_any()
        }
    };
    custom(tag)
        .class(classes)
        .attr("href", href)
        .on(leptos::ev::click, move |ev: MouseEvent| {
            if disabled.get_untracked() {
                return;
            }
            if let Some(cb) = on_click {
                cb.run(ev);
            }
        })
        .child(inner)
}

/// lodash `_.startCase`: words split on non-alphanumerics / case changes, capitalised, space-joined.
fn start_case(s: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut prev: Option<char> = None;
    for ch in s.chars() {
        if !ch.is_alphanumeric() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            prev = None;
            continue;
        }
        let boundary = match prev {
            Some(p) => {
                (p.is_lowercase() && ch.is_uppercase()) || (p.is_alphabetic() != ch.is_alphabetic())
            }
            None => false,
        };
        if boundary && !current.is_empty() {
            words.push(std::mem::take(&mut current));
        }
        current.push(ch);
        prev = Some(ch);
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
        .iter()
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `Menu.Menu`: `<div class="{position} menu">`.
#[component]
pub fn MenuMenu(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] position: Option<&'static str>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        Classes::new("")
            .opt(position)
            .add("menu")
            .add(&class.get())
            .build()
    };
    view! { <div class=classes>{children.map(|c| c())}</div> }
}

/// `Menu.Header`: `<div class="header">`.
#[component]
pub fn MenuHeader(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || Classes::new("header").add(&class.get()).build();
    view! { <div class=classes>{children.map(|c| c())}</div> }
}
