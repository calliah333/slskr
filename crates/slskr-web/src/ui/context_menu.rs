//! `ContextMenu`: a menu at a viewport position (right-click, the context-menu key or a
//! "⋯" button). Open it by setting its state signal; it closes on item click, Escape, Tab,
//! a click outside or scrolling, and returns focus to where it was.

use leptos::{ev::MouseEvent, prelude::*};
use wasm_bindgen::{JsCast, JsValue};

use super::icon::Icon;

/// One menu entry.
#[derive(Clone)]
pub enum MenuEntry {
    Item {
        label: String,
        icon: &'static str,
        disabled: bool,
        on_select: Callback<()>,
    },
    Separator,
}

impl MenuEntry {
    pub fn item(
        label: impl Into<String>,
        icon: &'static str,
        on_select: impl Fn() + Send + Sync + 'static,
    ) -> Self {
        Self::Item {
            label: label.into(),
            icon,
            disabled: false,
            on_select: Callback::new(move |()| on_select()),
        }
    }

    /// Greys the item out when `disabled`.
    pub fn disabled(self, disabled: bool) -> Self {
        match self {
            Self::Item {
                label,
                icon,
                on_select,
                ..
            } => Self::Item {
                label,
                icon,
                disabled,
                on_select,
            },
            separator => separator,
        }
    }
}

/// An open context menu: viewport coordinates of its top left corner and its entries.
#[derive(Clone)]
pub struct ContextMenuState {
    pub x: f64,
    pub y: f64,
    pub label: String,
    pub entries: Vec<MenuEntry>,
}

impl ContextMenuState {
    /// At the mouse pointer (a `contextmenu` event).
    pub fn at_pointer(
        event: &MouseEvent,
        label: impl Into<String>,
        entries: Vec<MenuEntry>,
    ) -> Self {
        Self {
            x: f64::from(event.client_x()),
            y: f64::from(event.client_y()),
            label: label.into(),
            entries,
        }
    }

    /// Below `element`, aligned to its left edge (a "⋯" button or a focused row).
    pub fn below(
        element: &web_sys::Element,
        label: impl Into<String>,
        entries: Vec<MenuEntry>,
    ) -> Self {
        let (x, y) = rect(element).map_or((0.0, 0.0), |(left, bottom)| (left, bottom + 4.0));
        Self {
            x,
            y,
            label: label.into(),
            entries,
        }
    }
}

/// `getBoundingClientRect()` left and bottom, without the `DomRect` web-sys feature.
fn rect(element: &web_sys::Element) -> Option<(f64, f64)> {
    let function = js_sys::Reflect::get(element, &JsValue::from_str("getBoundingClientRect"))
        .ok()?
        .dyn_into::<js_sys::Function>()
        .ok()?;
    let rect = function.call0(element).ok()?;
    let read = |name: &str| {
        js_sys::Reflect::get(&rect, &JsValue::from_str(name))
            .ok()
            .and_then(|value| value.as_f64())
    };
    Some((read("left")?, read("bottom")?))
}

fn menu_items(menu: &web_sys::Element) -> Vec<web_sys::HtmlElement> {
    // Walk the children (the `NodeList` web-sys feature is not enabled).
    std::iter::successors(menu.first_element_child(), |element| {
        element.next_element_sibling()
    })
    .filter(|element| {
        element.class_list().contains("menu__item") && !element.has_attribute("disabled")
    })
    .filter_map(|element| element.dyn_into::<web_sys::HtmlElement>().ok())
    .collect()
}

/// Renders the menu while `state` holds one.
#[component]
pub fn ContextMenu(state: RwSignal<Option<ContextMenuState>>) -> impl IntoView {
    let menu = NodeRef::<leptos::html::Div>::new();
    let restore = StoredValue::new_local(None::<web_sys::HtmlElement>);
    let close = move || {
        if state.with_untracked(Option::is_some) {
            state.set(None);
            if let Some(element) = restore.get_value() {
                let _ = element.focus();
            }
        }
    };
    let outside = window_event_listener(leptos::ev::mousedown, move |event: MouseEvent| {
        let target = event
            .target()
            .and_then(|t| t.dyn_into::<web_sys::Node>().ok());
        if let (Some(menu), Some(target)) = (menu.get_untracked(), target)
            && !menu.contains(Some(&target))
        {
            state.set(None);
        }
    });
    let scroll = window_event_listener(leptos::ev::scroll, move |_| {
        if state.with_untracked(Option::is_some) {
            state.set(None);
        }
    });
    let resize = window_event_listener(leptos::ev::resize, move |_| state.set(None));
    on_cleanup(move || {
        outside.remove();
        scroll.remove();
        resize.remove();
    });
    // Focus the first item when the menu opens.
    Effect::new(move |_| {
        if state.with(Option::is_some) {
            restore.set_value(
                document()
                    .active_element()
                    .and_then(|element| element.dyn_into::<web_sys::HtmlElement>().ok()),
            );
            if let Some(first) = menu
                .get()
                .and_then(|menu| menu_items(&menu).into_iter().next())
            {
                let _ = first.focus();
            }
        }
    });
    let on_keydown = move |event: web_sys::KeyboardEvent| {
        let Some(element) = menu.get_untracked() else {
            return;
        };
        let items = menu_items(&element);
        let active = document().active_element();
        let current = items.iter().position(|item| {
            active
                .as_ref()
                .is_some_and(|active| active == item.unchecked_ref::<web_sys::Element>())
        });
        let next = match event.key().as_str() {
            "ArrowDown" => current.map_or(0, |index| (index + 1) % items.len().max(1)),
            "ArrowUp" => current.map_or(items.len().saturating_sub(1), |index| {
                (index + items.len() - 1) % items.len().max(1)
            }),
            "Home" => 0,
            "End" => items.len().saturating_sub(1),
            "Escape" | "Tab" => {
                event.prevent_default();
                event.stop_propagation();
                close();
                return;
            }
            _ => return,
        };
        event.prevent_default();
        if let Some(item) = items.get(next) {
            let _ = item.focus();
        }
    };
    move || {
        state.get().map(|open| {
            // Keep the menu inside the viewport (estimated size; it is at most 20rem wide).
            let (width, height) = window_size();
            let estimated = 8.0 + 34.0 * open.entries.len() as f64;
            let x = open.x.min(width - 264.0).max(8.0);
            let y = if open.y + estimated > height - 8.0 {
                (open.y - estimated).max(8.0)
            } else {
                open.y
            };
            let entries = open
                .entries
                .into_iter()
                .map(|entry| match entry {
                    MenuEntry::Separator => view! { <div class="menu__separator" role="separator"></div> }.into_any(),
                    MenuEntry::Item {
                        label,
                        icon,
                        disabled,
                        on_select,
                    } => view! {
                        <button type="button" class="menu__item" role="menuitem" tabindex="-1" disabled=disabled
                            on:click=move |_| {
                                close();
                                on_select.run(());
                            }>
                            <Icon name=icon/>
                            <span>{label}</span>
                        </button>
                    }
                    .into_any(),
                })
                .collect_view();
            view! {
                <div class="menu menu--context" role="menu" aria-label=open.label node_ref=menu
                    style=format!("left: {x}px; top: {y}px")
                    on:keydown=on_keydown
                    on:contextmenu=|event: MouseEvent| event.prevent_default()>
                    {entries}
                </div>
            }
        })
    }
}

fn window_size() -> (f64, f64) {
    let window = window();
    let read =
        |value: Result<JsValue, JsValue>| value.ok().and_then(|v| v.as_f64()).unwrap_or(1024.0);
    (read(window.inner_width()), read(window.inner_height()))
}
