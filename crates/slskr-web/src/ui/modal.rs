//! `Modal`: a dialog over a dimmed backdrop, rendered into `document.body`.

use leptos::ev::{KeyboardEvent, MouseEvent};
use leptos::html::Div;
use leptos::portal::Portal;
use leptos::prelude::*;
use wasm_bindgen::JsCast;

use super::icon::Icon;

/// A footer button that runs its callback and then closes the modal.
#[derive(Clone, Debug, Default)]
pub struct ModalAction {
    pub content: String,
    /// `Button` variant: `"outline"` (default), `"primary"`, `"destructive"`.
    pub variant: &'static str,
    pub on_click: Option<Callback<()>>,
}

impl ModalAction {
    /// An outline button.
    pub fn new(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            variant: "outline",
            on_click: None,
        }
    }

    /// A solid red button (confirming a destructive action).
    pub fn negative(mut self) -> Self {
        self.variant = "destructive";
        self
    }

    /// A solid blue button.
    pub fn primary(mut self) -> Self {
        self.variant = "primary";
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

/// A modal dialog (`role="dialog"`, `aria-modal`): `.modal-backdrop > .modal` with a
/// header (title, description, close button), a scrolling body and a footer.
///
/// Open state is controlled by `open` when given, otherwise internal (opened by clicking
/// `trigger`; closed by the close button, a backdrop click, Escape or an action). The body
/// is `content` (a paragraph) or the children; the footer is `actions` or `footer`.
/// `size`: `"sm"` (24rem), default (32rem), `"lg"` (48rem), `"xl"` (64rem).
#[component]
pub fn Modal(
    #[prop(into, optional)] open: Option<Signal<bool>>,
    #[prop(into, optional)] trigger: Option<ViewFn>,
    #[prop(into, optional)] on_close: Option<Callback<()>>,
    #[prop(into)] title: String,
    #[prop(into, optional)] description: Option<String>,
    #[prop(optional)] size: Option<&'static str>,
    /// Show the close (x) button in the header (default true).
    #[prop(default = true)]
    close_button: bool,
    #[prop(into, optional)] content: Option<String>,
    #[prop(optional)] actions: Vec<ModalAction>,
    #[prop(optional)] footer: Option<ViewFn>,
    #[prop(into, optional)] class: Option<String>,
    #[prop(optional)] children: Option<ChildrenFn>,
) -> impl IntoView {
    let internal = RwSignal::new(false);
    let is_open = move || open.map_or_else(|| internal.get(), |o| o.get());
    let close = move || {
        internal.set(false);
        if let Some(cb) = on_close {
            cb.run(());
        }
    };
    let trigger_view = trigger.map(|t| {
        t.run()
            .add_any_attr(leptos::ev::on(leptos::ev::click, move |_: MouseEvent| {
                internal.set(true)
            }))
    });
    let parts = StoredValue::new((
        title,
        description,
        content,
        actions,
        footer,
        children,
        class,
    ));
    let title_id = StoredValue::new(format!("modal-title-{}", next_modal_id()));

    let body = move || {
        let modal_ref = NodeRef::<Div>::new();
        let down_inside = StoredValue::new(false);
        let previous_focus = document()
            .active_element()
            .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok());
        if let Some(body) = document().body() {
            let _ = body.class_list().add_1("modal-open");
        }
        let escape = window_event_listener(leptos::ev::keydown, move |ev: KeyboardEvent| {
            if ev.key() == "Escape" {
                close();
            }
        });
        let previous_focus = StoredValue::new_local(previous_focus);
        on_cleanup(move || {
            escape.remove();
            if let Some(body) = document().body() {
                let _ = body.class_list().remove_1("modal-open");
            }
            previous_focus.with_value(|el| {
                if let Some(el) = el {
                    let _ = el.focus();
                }
            });
        });
        // Focus the dialog itself so Escape and Tab start inside it.
        request_animation_frame(move || {
            if let Some(modal) = modal_ref.get_untracked() {
                let already_inside = document()
                    .active_element()
                    .is_some_and(|a| modal.contains(Some(&a)));
                if !already_inside {
                    let _ = modal.focus();
                }
            }
        });
        let inside = move |ev: &MouseEvent| {
            let target = ev.target().and_then(|t| t.dyn_into::<web_sys::Node>().ok());
            match (modal_ref.get_untracked(), target) {
                (Some(modal), Some(node)) => modal.contains(Some(&node)),
                _ => false,
            }
        };
        let on_mouse_down = move |ev: MouseEvent| down_inside.set_value(inside(&ev));
        let on_backdrop_click = move |ev: MouseEvent| {
            let started_inside = down_inside.get_value();
            down_inside.set_value(false);
            if !started_inside && !inside(&ev) {
                close();
            }
        };
        let (title, description, content, actions, footer, children, class) =
            parts.with_value(|p| {
                (
                    p.0.clone(),
                    p.1.clone(),
                    p.2.clone(),
                    p.3.clone(),
                    p.4.clone(),
                    p.5.clone(),
                    p.6.clone(),
                )
            });
        let classes = format!(
            "modal{}{}",
            size.map(|s| format!(" modal--{s}")).unwrap_or_default(),
            class.map(|c| format!(" {c}")).unwrap_or_default()
        );
        let body_view = match children {
            Some(children) => children().into_any(),
            None => content.map(|c| view! { <p>{c}</p> }).into_any(),
        };
        let footer_view = if !actions.is_empty() {
            let buttons = actions
                .into_iter()
                .map(|action| {
                    let callback = action.on_click;
                    view! {
                        <button type="button" class=format!("btn btn--{}", action.variant)
                            on:click=move |_| {
                                if let Some(cb) = callback {
                                    cb.run(());
                                }
                                close();
                            }>
                            {action.content}
                        </button>
                    }
                })
                .collect_view();
            Some(view! { <div class="modal__footer">{buttons}</div> }.into_any())
        } else {
            footer.map(|f| view! { <div class="modal__footer">{f.run()}</div> }.into_any())
        };
        let (title_id, labelled_by) = (title_id.get_value(), title_id.get_value());
        view! {
            <div class="modal-backdrop" on:mousedown=on_mouse_down on:click=on_backdrop_click>
                <div class=classes node_ref=modal_ref role="dialog" aria-modal="true"
                    aria-labelledby=labelled_by tabindex="-1">
                    <div class="modal__header">
                        <div>
                            <h2 class="modal__title" id=title_id>{title}</h2>
                            {description.map(|d| view! { <p class="modal__description">{d}</p> })}
                        </div>
                        {close_button.then(|| view! {
                            <button type="button" class="btn btn--ghost btn--icon btn--sm modal__close"
                                aria-label="Close" title="Close" on:click=move |_| close()>
                                <Icon name="x"/>
                            </button>
                        })}
                    </div>
                    <div class="modal__body">{body_view}</div>
                    {footer_view}
                </div>
            </div>
        }
    };

    view! {
        {trigger_view}
        <Show when=is_open>
            <Portal>{body()}</Portal>
        </Show>
    }
}

fn next_modal_id() -> usize {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    NEXT.fetch_add(1, Ordering::Relaxed)
}
