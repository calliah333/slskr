//! Stand-alone controls: `Checkbox`, `Switch`, `Tabs`, `DropdownMenu`.
//!
//! Form controls bound to a labelled field (settings, setup) are in [`super::form`]. Plain
//! inputs need no component: `<input class="input">`, `<select class="select">`,
//! `<textarea class="textarea">`.

use leptos::ev::{KeyboardEvent, MouseEvent};
use leptos::html::Div;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::icon::Icon;
use super::join_classes;

/// A native checkbox with its label: `<label class="check"><input class="checkbox">…`.
/// Without `label` give `aria_label`.
#[component]
pub fn Checkbox(
    #[prop(into)] checked: Signal<bool>,
    #[prop(into)] on_change: Callback<bool>,
    #[prop(into, optional)] label: Option<Signal<String>>,
    #[prop(into, optional)] aria_label: Option<String>,
    #[prop(into, optional)] disabled: Signal<bool>,
    #[prop(into, optional)] id: Option<String>,
) -> impl IntoView {
    view! {
        <label class="check">
            <input
                type="checkbox"
                class="checkbox"
                id=id
                aria-label=aria_label
                disabled=move || disabled.get()
                prop:checked=move || checked.get()
                on:change=move |ev| on_change.run(event_target_checked(&ev))
            />
            {label.map(|l| view! { <span>{move || l.get()}</span> })}
        </label>
    }
}

/// An on/off switch with its label (`role="switch"`).
#[component]
pub fn Switch(
    #[prop(into)] checked: Signal<bool>,
    #[prop(into)] on_change: Callback<bool>,
    #[prop(into, optional)] label: Option<Signal<String>>,
    #[prop(into, optional)] aria_label: Option<String>,
    #[prop(into, optional)] disabled: Signal<bool>,
    #[prop(into, optional)] id: Option<String>,
    #[prop(into, optional)] class: Signal<String>,
) -> impl IntoView {
    let classes = move || join_classes(&["switch", &class.get()]);
    view! {
        <label class=classes>
            <input
                type="checkbox"
                role="switch"
                id=id
                aria-label=aria_label
                disabled=move || disabled.get()
                prop:checked=move || checked.get()
                on:change=move |ev| on_change.run(event_target_checked(&ev))
            />
            <span class="switch__track" aria-hidden="true"></span>
            {label.map(|l| view! { <span class="switch__label">{move || l.get()}</span> })}
        </label>
    }
}

/// One tab of [`Tabs`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TabItem {
    pub value: String,
    pub label: String,
    pub icon: Option<&'static str>,
}

impl TabItem {
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            icon: None,
        }
    }

    pub fn icon(mut self, icon: &'static str) -> Self {
        self.icon = Some(icon);
        self
    }
}

/// Button tabs that switch content in place (`role="tablist"`). The segmented control by
/// default; `underline` for sub-section tabs. For tabs that change the URL, write links
/// with the same classes instead (see docs/UI.md).
#[component]
pub fn Tabs(
    items: Vec<TabItem>,
    #[prop(into)] value: Signal<String>,
    #[prop(into)] on_change: Callback<String>,
    #[prop(optional)] underline: bool,
    /// Accessible name of the tab list.
    #[prop(into)]
    label: String,
) -> impl IntoView {
    let class = if underline { "tabs-underline" } else { "tabs" };
    view! {
        <div class=class role="tablist" aria-label=label>
            {items.into_iter().map(|item| {
                let selected = {
                    let v = item.value.clone();
                    Memo::new(move |_| value.with(|current| *current == v))
                };
                let selected = move || selected.get();
                let pick = item.value.clone();
                view! {
                    <button type="button" class="tabs__tab" role="tab"
                        aria-selected=move || selected().to_string()
                        tabindex=move || if selected() { "0" } else { "-1" }
                        on:click=move |_| on_change.run(pick.clone())
                        on:keydown=move |ev: KeyboardEvent| move_tab_focus(&ev)>
                        {item.icon.map(|name| view! { <Icon name=name/> })}
                        {item.label}
                    </button>
                }
            }).collect_view()}
        </div>
    }
}

/// Arrow keys move focus between sibling tabs (and activate them by clicking).
fn move_tab_focus(ev: &KeyboardEvent) {
    let forward = match ev.key().as_str() {
        "ArrowRight" | "ArrowDown" => true,
        "ArrowLeft" | "ArrowUp" => false,
        _ => return,
    };
    let Some(current) = ev
        .current_target()
        .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
    else {
        return;
    };
    let next = if forward {
        current.next_element_sibling()
    } else {
        current.previous_element_sibling()
    };
    if let Some(next) = next.and_then(|n| n.dyn_into::<web_sys::HtmlElement>().ok()) {
        ev.prevent_default();
        let _ = next.focus();
        next.click();
    }
}

/// A button that opens a popover menu. Children are the menu entries, usually
/// `<button class="menu__item" role="menuitem">`; any click inside the menu closes it, as
/// do Escape and clicks outside.
///
/// `icon` without `text` makes an icon-only trigger (e.g. `"ellipsis"` for row actions);
/// `label` is then its accessible name. `align`: `"start"` (default) or `"end"`.
#[component]
pub fn DropdownMenu(
    #[prop(into)] label: String,
    #[prop(into, optional)] text: Option<Signal<String>>,
    #[prop(optional)] icon: Option<&'static str>,
    /// Trigger button variant (see `Button`); default outline, `"action"` for row menus.
    #[prop(optional)]
    variant: Option<&'static str>,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(optional)] align: Option<&'static str>,
    children: ChildrenFn,
) -> impl IntoView {
    let open = RwSignal::new(false);
    let root = NodeRef::<Div>::new();
    let icon_only = text.is_none();
    let trigger_class = join_classes(&[
        "btn",
        &variant.map(|v| format!("btn--{v}")).unwrap_or_default(),
        &size.map(|s| format!("btn--{s}")).unwrap_or_default(),
        if icon_only { "btn--icon" } else { "" },
    ]);
    let menu_class = if align == Some("end") {
        "menu menu--end"
    } else {
        "menu"
    };
    let outside = window_event_listener(leptos::ev::mousedown, move |ev: MouseEvent| {
        if !open.get_untracked() {
            return;
        }
        let target = ev.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok());
        if let (Some(root), Some(target)) = (root.get_untracked(), target)
            && !root.contains(Some(&target))
        {
            open.set(false);
        }
    });
    let escape = window_event_listener(leptos::ev::keydown, move |ev: KeyboardEvent| {
        if ev.key() == "Escape" && open.get_untracked() {
            open.set(false);
        }
    });
    on_cleanup(move || {
        outside.remove();
        escape.remove();
    });
    let aria = label.clone();
    view! {
        <div class="dropdown" node_ref=root>
            <button type="button" class=trigger_class aria-haspopup="menu"
                aria-expanded=move || open.get().to_string()
                aria-label=icon_only.then(|| aria.clone()) title=icon_only.then_some(label)
                on:click=move |_| open.update(|o| *o = !*o)>
                {icon.map(|name| view! { <Icon name=name/> })}
                {move || text.map(|t| view! { <span>{t.get()}</span> })}
                {(!icon_only).then(|| view! { <Icon name="chevron-down" size="sm" class="muted"/> })}
            </button>
            <Show when=move || open.get()>
                <div class=menu_class role="menu" on:click=move |_| open.set(false)>
                    {children()}
                </div>
            </Show>
        </div>
    }
}
