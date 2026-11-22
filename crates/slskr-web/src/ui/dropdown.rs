//! `Dropdown` (selection / search / multiple / button variants) and its parts.

use leptos::ev::{FocusEvent, KeyboardEvent, MouseEvent};
use leptos::html::{Div, Input};
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::classes::{Classes, KeyOrValue, client_rect, emit};

/// One entry of a dropdown's `options`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DropdownOption {
    pub key: String,
    pub text: String,
    pub value: String,
    pub description: Option<String>,
    pub icon: Option<String>,
    pub flag: Option<String>,
    pub disabled: bool,
}

impl DropdownOption {
    /// `{ key, text, value }`.
    pub fn new(key: impl Into<String>, text: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            key: key.into(),
            text: text.into(),
            value: value.into(),
            ..Self::default()
        }
    }
}

/// semantic-ui-react `Dropdown`.
///
/// Single selection reports through `on_change` (controlled by `value` when given);
/// `multiple` selection through `on_change_multiple` (controlled by `values`).
/// `text` overrides the displayed text, `trigger` replaces it entirely.
/// Keyboard: arrows move/select, Enter/Space select, Escape closes.
#[component]
pub fn Dropdown(
    #[prop(into, optional)] options: Signal<Vec<DropdownOption>>,
    #[prop(into, optional)] value: Option<Signal<String>>,
    #[prop(into, optional)] values: Option<Signal<Vec<String>>>,
    #[prop(into, optional)] default_value: Option<String>,
    #[prop(into, optional)] on_change: Option<Callback<String>>,
    #[prop(into, optional)] on_change_multiple: Option<Callback<Vec<String>>>,
    #[prop(into, optional)] on_open: Option<Callback<()>>,
    #[prop(into, optional)] on_close: Option<Callback<()>>,
    #[prop(into, optional)] placeholder: Option<String>,
    #[prop(into, optional)] text: Option<Signal<String>>,
    /// Icon name (default `"dropdown"`).
    #[prop(optional)]
    icon: Option<&'static str>,
    #[prop(into, optional)] trigger: Option<ViewFn>,
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] basic: bool,
    #[prop(optional)] button: bool,
    #[prop(optional)] compact: bool,
    #[prop(optional)] fluid: bool,
    #[prop(optional)] floating: bool,
    #[prop(optional)] inline: bool,
    #[prop(optional)] item: bool,
    #[prop(optional)] labeled: bool,
    #[prop(optional)] multiple: bool,
    #[prop(optional)] search: bool,
    #[prop(optional)] selection: bool,
    #[prop(optional)] simple: bool,
    #[prop(optional)] scrolling: bool,
    #[prop(into, optional)] pointing: KeyOrValue,
    #[prop(into, optional)] error: Signal<bool>,
    #[prop(into, optional)] disabled: Signal<bool>,
    #[prop(into, optional)] loading: Signal<bool>,
    /// Menu direction (`"left"` / `"right"`).
    #[prop(optional)]
    direction: Option<&'static str>,
    #[prop(into, default = "No results found.".to_string())] no_results_message: String,
    #[prop(default = true)] select_on_navigation: bool,
) -> impl IntoView {
    let root = NodeRef::<Div>::new();
    let search_ref = NodeRef::<Input>::new();
    let open = RwSignal::new(false);
    let focus = RwSignal::new(false);
    let upward = RwSignal::new(false);
    let query = RwSignal::new(String::new());
    let highlighted = RwSignal::new(None::<usize>);
    let selected_label = RwSignal::new(None::<String>);
    let internal_value = RwSignal::new(default_value.unwrap_or_default());
    let internal_values = RwSignal::new(Vec::<String>::new());
    let mouse_down = StoredValue::new(false);

    let current_value = move || value.map_or_else(|| internal_value.get(), |v| v.get());
    let current_values = move || values.map_or_else(|| internal_values.get(), |v| v.get());
    let has_value = move || {
        if multiple {
            !current_values().is_empty()
        } else {
            !current_value().is_empty()
        }
    };

    let menu_options = move || -> Vec<DropdownOption> {
        let selected = if multiple {
            current_values()
        } else {
            Vec::new()
        };
        let q = query.get().to_lowercase();
        options
            .get()
            .into_iter()
            .filter(|o| !multiple || !selected.contains(&o.value))
            .filter(|o| !search || q.is_empty() || o.text.to_lowercase().contains(&q))
            .collect()
    };
    let selected_index = move || {
        let opts = menu_options();
        highlighted.get().filter(|i| *i < opts.len()).or_else(|| {
            let current = (!multiple).then(current_value);
            current
                .and_then(|v| opts.iter().position(|o| o.value == v))
                .or_else(|| opts.iter().position(|o| !o.disabled))
        })
    };

    let set_open_direction = move || {
        let Some(el) = root.get_untracked() else {
            return;
        };
        let Ok(Some(menu)) = el.query_selector(".menu.visible") else {
            return;
        };
        let rect = client_rect(&el);
        let menu_height = f64::from(menu.client_height());
        let viewport = document()
            .document_element()
            .map(|d| f64::from(d.client_height()))
            .unwrap_or_default();
        let bottom = viewport - rect.top - rect.height - menu_height;
        let top = rect.top - menu_height;
        let is_upward = bottom < 0.0 && top > bottom;
        if upward.get_untracked() != is_upward {
            upward.set(is_upward);
        }
    };

    let do_open = move || {
        if disabled.get_untracked() {
            return;
        }
        if search && let Some(input) = search_ref.get_untracked() {
            let _ = input.focus();
        }
        emit(on_open, ());
        open.set(true);
        request_animation_frame(set_open_direction);
    };
    // `blur_root`: semantic-ui-react's default `handleClose` callback (skipped by close-on-change).
    let do_close = move |blur_root: bool| {
        if !open.get_untracked() {
            return;
        }
        emit(on_close, ());
        open.set(false);
        if blur_root {
            let active = document().active_element();
            let search_focused = matches!(
                (&active, search_ref.get_untracked()),
                (Some(a), Some(s)) if a == s.unchecked_ref::<web_sys::Element>()
            );
            if !search_focused && let Some(el) = root.get_untracked() {
                let _ = el.blur();
            }
            let root_focused = matches!(
                (document().active_element(), root.get_untracked()),
                (Some(a), Some(r)) if &a == r.unchecked_ref::<web_sys::Element>()
            );
            focus.set(search_focused || root_focused);
        }
    };
    let toggle = move || {
        if open.get_untracked() {
            do_close(true)
        } else {
            do_open()
        }
    };
    let clear_query = move || {
        if !query.get_untracked().is_empty() {
            query.set(String::new());
        }
    };

    // Sets the value from a menu option; returns true when it changed.
    let make_active = move |opt: &DropdownOption| -> bool {
        if opt.disabled {
            return false;
        }
        if multiple {
            let mut next = current_values();
            if next.contains(&opt.value) {
                return false;
            }
            next.push(opt.value.clone());
            internal_values.set(next.clone());
            emit(on_change_multiple, next);
        } else {
            if current_value() == opt.value {
                return false;
            }
            internal_value.set(opt.value.clone());
            emit(on_change, opt.value.clone());
        }
        highlighted.set(None);
        true
    };

    let handle_item_click = move |ev: MouseEvent, opt: DropdownOption| {
        ev.stop_propagation();
        if opt.disabled {
            return;
        }
        untrack(|| {
            make_active(&opt);
            if !multiple {
                do_close(false);
            }
            clear_query();
        });
        if search && let Some(input) = search_ref.get_untracked() {
            let _ = input.focus();
        }
    };

    let handle_click = move |ev: MouseEvent| {
        ev.stop_propagation();
        untrack(|| {
            if !search {
                toggle();
            } else if open.get() {
                if let Some(input) = search_ref.get() {
                    let _ = input.focus();
                }
            } else {
                do_open();
            }
        });
    };

    let handle_icon_click = move |ev: MouseEvent| {
        ev.stop_propagation();
        untrack(toggle);
    };

    let handle_key_down = move |ev: KeyboardEvent| {
        untrack(|| {
            let key = ev.key();
            let was_open = open.get();
            let has_focus = focus.get();
            let target_is_input = ev
                .target()
                .and_then(|t| t.dyn_into::<web_sys::Element>().ok())
                .is_some_and(|el| matches!(el.tag_name().as_str(), "INPUT" | "TEXTAREA"));
            match key.as_str() {
                "ArrowDown" | "ArrowUp" if was_open => {
                    ev.prevent_default();
                    let opts = menu_options();
                    if opts.is_empty() || opts.iter().all(|o| o.disabled) {
                        return;
                    }
                    let step: isize = if key == "ArrowDown" { 1 } else { -1 };
                    let len = opts.len() as isize;
                    let mut next = selected_index().map_or(0, |i| i as isize);
                    loop {
                        next = (next + step).rem_euclid(len);
                        if !opts[next as usize].disabled {
                            break;
                        }
                    }
                    let next = next as usize;
                    if !multiple && select_on_navigation {
                        make_active(&opts[next]);
                    }
                    highlighted.set(Some(next));
                }
                "ArrowDown" | "ArrowUp" if has_focus => {
                    ev.prevent_default();
                    do_open();
                }
                "Escape" if was_open => {
                    ev.prevent_default();
                    do_close(true);
                }
                " " if has_focus && !was_open => {
                    if !target_is_input {
                        ev.prevent_default();
                    }
                    do_open();
                }
                "Enter" | " " if was_open && (key == "Enter" || !search) => {
                    ev.prevent_default();
                    let opts = menu_options();
                    if search && opts.is_empty() {
                        return;
                    }
                    if let Some(opt) = selected_index().and_then(|i| opts.get(i).cloned()) {
                        make_active(&opt);
                    }
                    if !multiple {
                        do_close(false);
                    }
                    clear_query();
                }
                "Backspace" if search && multiple && query.get().is_empty() => {
                    let mut next = current_values();
                    if next.pop().is_some() {
                        ev.prevent_default();
                        internal_values.set(next.clone());
                        emit(on_change_multiple, next);
                    }
                }
                _ => {}
            }
        });
    };

    let handle_focus = move |_: FocusEvent| {
        if !focus.get_untracked() {
            focus.set(true);
        }
    };
    let handle_blur = move |ev: FocusEvent| {
        untrack(|| {
            let related = ev
                .related_target()
                .and_then(|t| t.dyn_into::<web_sys::Node>().ok());
            if let (Some(el), Some(node)) = (root.get(), related.as_ref())
                && el.contains(Some(node))
            {
                return;
            }
            if mouse_down.get_value() {
                return;
            }
            if !multiple && open.get() {
                let opts = menu_options();
                if let Some(opt) = selected_index().and_then(|i| opts.get(i).cloned()) {
                    make_active(&opt);
                }
                do_close(true);
            }
            focus.set(false);
            clear_query();
        });
    };

    let mouse_up = window_event_listener(leptos::ev::mouseup, move |_| mouse_down.set_value(false));
    let document_click = window_event_listener(leptos::ev::click, move |ev: MouseEvent| {
        if !open.get_untracked() {
            return;
        }
        let target = ev.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok());
        if let (Some(el), Some(node)) = (root.get_untracked(), target.as_ref())
            && el.contains(Some(node))
        {
            return;
        }
        untrack(|| do_close(true));
    });
    on_cleanup(move || {
        mouse_up.remove();
        document_click.remove();
    });

    let classes = move || {
        Classes::new("ui")
            .key(open.get(), "active visible")
            .key(disabled.get(), "disabled")
            .key(error.get(), "error")
            .key(loading.get(), "loading")
            .key(basic, "basic")
            .key(button, "button")
            .key(compact, "compact")
            .key(fluid, "fluid")
            .key(floating, "floating")
            .key(inline, "inline")
            .key(labeled, "labeled")
            .key(item, "item")
            .key(multiple, "multiple")
            .key(search, "search")
            .key(selection, "selection")
            .key(simple, "simple")
            .key(scrolling, "scrolling")
            .key(upward.get(), "upward")
            .key_or_value(pointing, "pointing")
            .add("dropdown")
            .add(&class.get())
            .build()
    };
    let tab_index = move || {
        if search {
            None
        } else if disabled.get() {
            Some(-1)
        } else {
            Some(0)
        }
    };

    let labels = move || {
        multiple.then(|| {
            let all = options.get();
            current_values()
                .into_iter()
                .filter_map(|v| all.iter().find(|o| o.value == v).cloned())
                .map(|opt| {
                    let value = opt.value.clone();
                    let active = selected_label.get().as_deref() == Some(value.as_str());
                    let label_value = value.clone();
                    let remove_value = value.clone();
                    let on_label_click = move |ev: MouseEvent| {
                        ev.stop_propagation();
                        selected_label.set(Some(label_value.clone()));
                    };
                    let on_remove = move |ev: MouseEvent| {
                        ev.stop_propagation();
                        let next: Vec<String> =
                            untrack(current_values).into_iter().filter(|v| *v != remove_value).collect();
                        internal_values.set(next.clone());
                        emit(on_change_multiple, next);
                    };
                    view! {
                        <a class={if active { "ui active label" } else { "ui label" }} data-value=value on:click=on_label_click>
                            {opt.flag.clone().map(|f| view! { <i class=format!("{f} flag")></i> })}
                            {opt.text.clone()}
                            <i aria-hidden="true" class="delete icon" on:click=on_remove></i>
                        </a>
                    }
                })
                .collect_view()
        })
    };

    let search_input = search.then(|| {
        view! {
            <input
                aria-autocomplete="list"
                autocomplete="off"
                class="search"
                tabindex=move || if disabled.get() { -1 } else { 0 }
                type="text"
                node_ref=search_ref
                prop:value=move || query.get()
                on:input=move |ev| {
                    ev.stop_propagation();
                    let q = event_target_value(&ev);
                    let was_open = open.get_untracked();
                    query.set(q.clone());
                    highlighted.set(Some(0));
                    if !was_open && !q.is_empty() {
                        untrack(do_open);
                    }
                }
            />
        }
    });
    let sizer = (search && multiple).then(|| view! { <span class="sizer"></span> });

    let text_view = match trigger {
        Some(trigger) => trigger.run(),
        None => {
            let has_placeholder = placeholder.is_some();
            let display = move || -> Option<String> {
                if let Some(t) = text {
                    return Some(t.get());
                }
                let item = if open.get() && !multiple {
                    let opts = menu_options();
                    selected_index().and_then(|i| opts.get(i).cloned())
                } else if has_value() && !multiple {
                    let v = current_value();
                    options.get().into_iter().find(|o| o.value == v)
                } else {
                    None
                };
                item.map(|o| o.text).or_else(|| placeholder.clone())
            };
            let text_classes = move || {
                Classes::new("divider")
                    .key(has_placeholder && !has_value(), "default")
                    .add("text")
                    .key(search && !query.get().is_empty(), "filtered")
                    .build()
            };
            (move || {
                display().map(|d| {
                    view! { <div aria-atomic="true" aria-live="polite" role="alert" class=text_classes>{d}</div> }
                })
            })
            .into_any()
        }
    };

    let menu_classes = move || {
        Classes::new("")
            .opt(direction)
            .key(open.get(), "visible")
            .add("menu transition")
            .build()
    };
    let items = move || {
        let opts = menu_options();
        if search && opts.is_empty() {
            return view! { <div class="message">{no_results_message.clone()}</div> }.into_any();
        }
        let selected = selected_index();
        let single = (!multiple).then(current_value);
        opts.into_iter()
            .enumerate()
            .map(|(i, opt)| {
                let is_active = single.as_deref() == Some(opt.value.as_str());
                let is_selected = selected == Some(i);
                let item_classes = Classes::new("")
                    .key(is_active, "active")
                    .key(opt.disabled, "disabled")
                    .key(is_selected, "selected")
                    .add("item")
                    .build();
                let click_opt = opt.clone();
                view! {
                    <div
                        style="pointer-events: all;"
                        role="option"
                        aria-checked=is_active.to_string()
                        aria-disabled=opt.disabled.then_some("true")
                        aria-selected=is_selected.to_string()
                        class=item_classes
                        on:click=move |ev| handle_item_click(ev, click_opt.clone())
                    >
                        {opt.icon.clone().map(|name| view! { <i aria-hidden="true" class=format!("{name} icon")></i> })}
                        {opt.flag.clone().map(|f| view! { <i class=format!("{f} flag")></i> })}
                        {opt.description.clone().map(|d| view! { <span class="description">{d}</span> })}
                        <span class="text">{opt.text.clone()}</span>
                    </div>
                }
            })
            .collect_view()
            .into_any()
    };

    view! {
        <div
            node_ref=root
            role=if search { "combobox" } else { "listbox" }
            aria-busy=move || loading.get().to_string()
            aria-disabled=move || disabled.get().to_string()
            aria-expanded=move || open.get().to_string()
            aria-multiselectable=(!search && multiple).then_some("true")
            class=classes
            tabindex=tab_index
            on:click=handle_click
            on:keydown=handle_key_down
            on:mousedown=move |_| mouse_down.set_value(true)
            on:focusin=handle_focus
            on:focusout=handle_blur
        >
            {labels}
            {search_input}
            {sizer}
            {text_view}
            <i aria-hidden="true" class=format!("{} icon", icon.unwrap_or("dropdown")) on:click=handle_icon_click></i>
            <div
                class=menu_classes
                role=search.then_some("listbox")
                aria-multiselectable=(search && multiple).then_some("true")
            >
                {items}
            </div>
        </div>
    }
}

/// `Dropdown.Menu`: `<div class="{direction} [visible] menu transition">`.
#[component]
pub fn DropdownMenu(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] direction: Option<&'static str>,
    #[prop(into, optional)] open: Signal<bool>,
    #[prop(optional)] scrolling: bool,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        Classes::new("")
            .opt(direction)
            .key(open.get(), "visible")
            .key(scrolling, "scrolling")
            .add("menu transition")
            .add(&class.get())
            .build()
    };
    view! { <div class=classes>{children.map(|c| c())}</div> }
}

/// `Dropdown.Item`: `<div role="option" class="[active] [selected] item">`.
#[component]
pub fn DropdownItem(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(into, optional)] active: Signal<bool>,
    #[prop(into, optional)] disabled: Signal<bool>,
    #[prop(into, optional)] selected: Signal<bool>,
    #[prop(optional)] icon: Option<&'static str>,
    #[prop(into, optional)] description: Option<String>,
    #[prop(into, optional)] text: Option<Signal<String>>,
    #[prop(into, optional)] on_click: Option<Callback<MouseEvent>>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        Classes::new("")
            .key(active.get(), "active")
            .key(disabled.get(), "disabled")
            .key(selected.get(), "selected")
            .add("item")
            .add(&class.get())
            .build()
    };
    let inner = match children {
        Some(children) => children(),
        None => view! {
            {icon.map(|name| view! { <i aria-hidden="true" class=format!("{name} icon")></i> })}
            {description.map(|d| view! { <span class="description">{d}</span> })}
            {text.map(|t| view! { <span class="text">{move || t.get()}</span> })}
        }
        .into_any(),
    };
    view! {
        <div
            role="option"
            aria-checked=move || active.get().to_string()
            aria-disabled=move || disabled.get().then_some("true")
            aria-selected=move || selected.get().to_string()
            class=classes
            on:click=move |ev| emit(on_click, ev)
        >
            {inner}
        </div>
    }
}

/// `Dropdown.Divider`: `<div class="divider">`.
#[component]
pub fn DropdownDivider(#[prop(into, optional)] class: Signal<String>) -> impl IntoView {
    let classes = move || Classes::new("divider").add(&class.get()).build();
    view! { <div class=classes></div> }
}

/// `Dropdown.Header`: `<div class="header">`.
#[component]
pub fn DropdownHeader(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || Classes::new("header").add(&class.get()).build();
    view! { <div class=classes>{children.map(|c| c())}</div> }
}
