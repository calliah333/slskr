//! `Modal` (rendered through a portal into `document.body`) and its sub-components.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use leptos::ev::{KeyboardEvent, MouseEvent};
use leptos::html::Div;
use leptos::portal::Portal as LeptosPortal;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::button::Button;
use super::classes::{Classes, KeyOrValue, client_rect, emit, toggle_body_classes};
use super::icon::Icon;

/// A shorthand action button (`actions={['Cancel', { content, negative, onClick }]}`).
/// Clicking any action runs its callback and then closes the modal.
#[derive(Clone, Debug, Default)]
pub struct ModalAction {
    pub content: String,
    pub negative: bool,
    pub positive: bool,
    pub primary: bool,
    pub on_click: Option<Callback<()>>,
}

impl ModalAction {
    /// A plain `<button class="ui button">` action.
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            ..Self::default()
        }
    }

    /// Adds the `negative` class.
    pub fn negative(mut self) -> Self {
        self.negative = true;
        self
    }

    /// Adds the `positive` class.
    pub fn positive(mut self) -> Self {
        self.positive = true;
        self
    }

    /// Adds the `primary` class.
    pub fn primary(mut self) -> Self {
        self.primary = true;
        self
    }

    /// Runs `f` when clicked (before the modal closes).
    pub fn on_click(mut self, f: impl Fn() + Send + Sync + 'static) -> Self {
        self.on_click = Some(Callback::new(move |()| f()));
        self
    }
}

impl From<&str> for ModalAction {
    fn from(value: &str) -> Self {
        ModalAction::new(value)
    }
}

impl From<String> for ModalAction {
    fn from(value: String) -> Self {
        ModalAction::new(value)
    }
}

struct ModalParts {
    children: Option<ChildrenFn>,
    header: Option<ViewFn>,
    content: Option<String>,
    actions: Vec<ModalAction>,
    style: Option<String>,
}

/// semantic-ui-react `Modal`.
///
/// Open state is controlled by `open` when given, otherwise internal (opened by
/// clicking `trigger`, closed by the close icon, a dimmer click, Escape or an action).
/// Renders `<div class="ui page modals dimmer transition visible active">` >
/// `<div class="ui {size} modal transition visible active">` into `document.body`.
/// `header` is rendered as given (e.g. `<Header icon=.. content=../>` or `<ModalHeader>`),
/// `content` is wrapped in `<div class="content">`.
#[component]
pub fn Modal(
    #[prop(into, optional)] open: Option<Signal<bool>>,
    #[prop(optional)] default_open: bool,
    #[prop(into, optional)] trigger: Option<ViewFn>,
    #[prop(into, optional)] on_open: Option<Callback<()>>,
    #[prop(into, optional)] on_close: Option<Callback<()>>,
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(optional)] basic: bool,
    #[prop(default = true)] centered: bool,
    /// `true` renders the `close` icon; a name renders that icon.
    #[prop(into, optional)]
    close_icon: KeyOrValue,
    #[prop(default = true)] close_on_dimmer_click: bool,
    #[prop(default = true)] close_on_escape: bool,
    /// `"inverted"` or `"blurring"`.
    #[prop(optional)]
    dimmer: Option<&'static str>,
    #[prop(into, optional)] header: Option<ViewFn>,
    #[prop(into, optional)] content: Option<String>,
    #[prop(optional)] actions: Vec<ModalAction>,
    #[prop(into, optional)] style: Option<String>,
    #[prop(optional)] children: Option<ChildrenFn>,
) -> impl IntoView {
    let internal = RwSignal::new(default_open);
    let is_open = move || open.map_or_else(|| internal.get(), |o| o.get());
    let do_open = move || {
        emit(on_open, ());
        internal.set(true);
    };
    let do_close = move || {
        internal.set(false);
        emit(on_close, ());
    };
    let parts = StoredValue::new(ModalParts {
        children,
        header,
        content,
        actions,
        style,
    });

    let trigger_view = trigger.map(|t| {
        t.run()
            .add_any_attr(leptos::ev::on(leptos::ev::click, move |_: MouseEvent| {
                if !is_open_untracked(open, internal) {
                    do_open();
                }
            }))
    });

    let body = move || {
        let modal_ref = NodeRef::<Div>::new();
        let scrolling = RwSignal::new(false);
        let mouse_down_inside = StoredValue::new(false);
        let alive = Arc::new(AtomicBool::new(true));

        let blurring = dimmer == Some("blurring");
        toggle_body_classes(&["dimmable", "dimmed"], true);
        if blurring {
            toggle_body_classes(&["blurring"], true);
        }
        Effect::new(move |_| toggle_body_classes(&["scrolling"], scrolling.get()));
        watch_fit(modal_ref, scrolling, alive.clone());

        let escape = close_on_escape.then(|| {
            window_event_listener(leptos::ev::keydown, move |ev: KeyboardEvent| {
                if ev.key() == "Escape" {
                    do_close();
                }
            })
        });
        on_cleanup(move || {
            alive.store(false, Ordering::Relaxed);
            if let Some(handle) = escape {
                handle.remove();
            }
            toggle_body_classes(&["dimmable", "dimmed", "blurring", "scrolling"], false);
        });

        let contains = move |ev: &MouseEvent| -> bool {
            let target = ev.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok());
            match (modal_ref.get_untracked(), target) {
                (Some(modal), Some(node)) => modal.contains(Some(&node)),
                _ => false,
            }
        };
        let on_mouse_down = move |ev: MouseEvent| mouse_down_inside.set_value(contains(&ev));
        let on_dimmer_click = move |ev: MouseEvent| {
            let down_inside = mouse_down_inside.get_value();
            mouse_down_inside.set_value(false);
            if !close_on_dimmer_click || down_inside || contains(&ev) {
                return;
            }
            do_close();
        };

        let dimmer_classes = Classes::new("ui")
            .key(dimmer == Some("inverted"), "inverted")
            .key(!centered, "top aligned")
            .add("page modals dimmer transition visible active")
            .build();
        let modal_classes = move || {
            Classes::new("ui")
                .opt(size)
                .key(basic, "basic")
                .key(scrolling.get(), "scrolling")
                .add("modal transition visible active")
                .add(&class.get())
                .build()
        };
        let close_icon_view = match close_icon {
            KeyOrValue::Off => None,
            KeyOrValue::On => Some("close"),
            KeyOrValue::Value(name) => Some(name),
        }
        .map(|name| view! { <Icon name=name on_click=move |_: MouseEvent| do_close() /> });

        let (inner, style) = parts.with_value(|p| {
            let inner = match &p.children {
                Some(children) => children(),
                None => {
                    let header = p.header.as_ref().map(|h| h.run());
                    let content = p
                        .content
                        .clone()
                        .map(|c| view! { <div class="content">{c}</div> });
                    let actions = (!p.actions.is_empty()).then(|| {
                        let buttons = p
                            .actions
                            .iter()
                            .cloned()
                            .map(|action| {
                                let callback = action.on_click;
                                view! {
                                    <Button
                                        negative=action.negative
                                        positive=action.positive
                                        primary=action.primary
                                        on_click=move |_: MouseEvent| {
                                            emit(callback, ());
                                            do_close();
                                        }
                                    >
                                        {action.content}
                                    </Button>
                                }
                            })
                            .collect_view();
                        view! { <div class="actions">{buttons}</div> }
                    });
                    view! { {header}{content}{actions} }.into_any()
                }
            };
            (inner, p.style.clone())
        });

        view! {
            <div
                class=dimmer_classes
                style="display: flex !important;"
                on:mousedown=on_mouse_down
                on:click=on_dimmer_click
            >
                <div class=modal_classes node_ref=modal_ref style=style>
                    {close_icon_view}
                    {inner}
                </div>
            </div>
        }
    };

    view! {
        {trigger_view}
        <Show when=is_open>
            <LeptosPortal>{body()}</LeptosPortal>
        </Show>
    }
}

fn is_open_untracked(open: Option<Signal<bool>>, internal: RwSignal<bool>) -> bool {
    open.map_or_else(|| internal.get_untracked(), |o| o.get_untracked())
}

/// Re-measures the modal every animation frame and toggles `scrolling` when it no
/// longer fits the viewport (semantic-ui-react `setPositionAndClassNames`).
fn watch_fit(modal_ref: NodeRef<Div>, scrolling: RwSignal<bool>, alive: Arc<AtomicBool>) {
    request_animation_frame(move || {
        if !alive.load(Ordering::Relaxed) {
            return;
        }
        if let Some(modal) = modal_ref.get_untracked() {
            let height = client_rect(&modal).height;
            let viewport = window()
                .inner_height()
                .ok()
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0);
            let fits = viewport / 2.0 - height / 2.0 + height + 50.0 < viewport;
            if scrolling.get_untracked() == fits {
                scrolling.set(!fits);
            }
        }
        watch_fit(modal_ref, scrolling, alive);
    });
}

/// `Modal.Header`: `<div class="header">`.
#[component]
pub fn ModalHeader(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || Classes::new("header").add(&class.get()).build();
    view! { <div class=classes>{children.map(|c| c())}</div> }
}

/// `Modal.Content`: `<div class="{class} [image] [scrolling] content">`.
#[component]
pub fn ModalContent(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] image: bool,
    #[prop(optional)] scrolling: bool,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        Classes::new(&class.get())
            .key(image, "image")
            .key(scrolling, "scrolling")
            .add("content")
            .build()
    };
    view! { <div class=classes>{children.map(|c| c())}</div> }
}

/// `Modal.Description`: `<div class="description">`.
#[component]
pub fn ModalDescription(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || Classes::new("description").add(&class.get()).build();
    view! { <div class=classes>{children.map(|c| c())}</div> }
}

/// `Modal.Actions`: `<div class="actions">`.
#[component]
pub fn ModalActions(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || Classes::new("actions").add(&class.get()).build();
    view! { <div class=classes>{children.map(|c| c())}</div> }
}
