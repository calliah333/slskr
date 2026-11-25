//! Status and feedback: `Alert`, `Badge`, `Loading`, `Progress`, `Toast`, `StatusDot`,
//! `Avatar`.

use leptos::prelude::*;

use super::icon::Icon;
use super::join_classes;

/// Maps the old Semantic UI message kinds to alert variants.
fn alert_variant(kind: &str) -> &'static str {
    match kind {
        "negative" | "error" | "danger" | "destructive" => "danger",
        "positive" | "success" => "success",
        "warning" => "warning",
        "info" => "info",
        _ => "",
    }
}

/// A bordered message box with a variant icon: `<div class="alert alert--{variant}">`.
///
/// `variant`: `"info"`, `"success"`, `"warning"`, `"danger"` (Semantic's `negative`,
/// `positive` and `error` are accepted too) or none for a neutral box. `role` defaults to
/// `alert` for danger and `status` otherwise. `on_dismiss` adds a close button.
#[component]
pub fn Alert(
    #[prop(optional)] variant: Option<&'static str>,
    #[prop(into, optional)] title: Option<String>,
    /// Overrides the variant's icon (`""` hides it).
    #[prop(optional)]
    icon: Option<&'static str>,
    #[prop(optional)] role: Option<&'static str>,
    #[prop(into, optional)] on_dismiss: Option<Callback<()>>,
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let variant = alert_variant(variant.unwrap_or(""));
    let icon = icon.unwrap_or(match variant {
        "danger" => "circle-alert",
        "success" => "circle-check",
        "warning" => "triangle-alert",
        _ => "info",
    });
    let modifier = (!variant.is_empty()).then(|| format!("alert--{variant}"));
    let classes = move || join_classes(&["alert", modifier.as_deref().unwrap_or(""), &class.get()]);
    let role = role.unwrap_or(if variant == "danger" {
        "alert"
    } else {
        "status"
    });
    view! {
        <div class=classes role=role>
            {(!icon.is_empty()).then(|| view! { <Icon name=icon/> })}
            <div class="alert__body">
                {title.map(|t| view! { <div class="alert__title">{t}</div> })}
                {children.map(|c| c())}
            </div>
            {on_dismiss.map(|cb| view! {
                <button type="button" class="btn btn--ghost btn--icon btn--sm alert__dismiss"
                    aria-label="Dismiss" title="Dismiss" on:click=move |_| cb.run(())>
                    <Icon name="x"/>
                </button>
            })}
        </div>
    }
}

/// A small pill: `<span class="badge badge--{variant}">`.
///
/// `variant`: `"neutral"` (default, slate), `"primary"`/`"blue"`, `"success"`/`"green"`,
/// `"warning"`/`"amber"`, `"danger"`/`"red"`, `"purple"`, `"outline"`, `"solid"`.
#[component]
pub fn Badge(
    #[prop(optional)] variant: Option<&'static str>,
    #[prop(optional)] icon: Option<&'static str>,
    #[prop(into, optional)] text: Option<Signal<String>>,
    #[prop(into, optional)] title: Option<String>,
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let modifier = variant
        .filter(|v| *v != "neutral")
        .map(|v| format!("badge--{v}"));
    let classes = move || join_classes(&["badge", modifier.as_deref().unwrap_or(""), &class.get()]);
    view! {
        <span class=classes title=title>
            {icon.map(|name| view! { <Icon name=name/> })}
            {children.map(|c| c())}
            {move || text.map(|t| t.get())}
        </span>
    }
}

/// A centered loading row (spinner + muted text) for a page or a card that is loading.
#[component]
pub fn Loading(
    /// Accessible name and visible text; default "Loading".
    #[prop(into, optional)]
    label: Option<String>,
    /// Fill half the viewport (whole-page loading).
    #[prop(optional)]
    page: bool,
    /// Show the label next to the spinner (otherwise screen-reader only).
    #[prop(optional)]
    show_label: bool,
) -> impl IntoView {
    let label = label.unwrap_or_else(|| "Loading".to_owned());
    let class = if page {
        "loading loading--page"
    } else {
        "loading"
    };
    let text_class = if show_label { "" } else { "sr-only" };
    view! {
        <div class=class role="status" aria-label=label.clone()>
            <span class="spinner spinner--lg" aria-hidden="true"></span>
            <span class=text_class>{label.clone()}</span>
        </div>
    }
}

/// A thin progress bar: `<div class="progress" role="progressbar">`.
///
/// `value: None` renders an indeterminate bar. `variant`: `"success"`, `"warning"`,
/// `"danger"`. `large` makes it 10px tall.
#[component]
pub fn Progress(
    #[prop(into, optional)] value: Option<Signal<f64>>,
    /// The value of a full bar (default 100).
    #[prop(optional)]
    total: f64,
    #[prop(into, optional)] variant: Option<Signal<&'static str>>,
    #[prop(optional)] large: bool,
    #[prop(into, optional)] label: Option<String>,
    #[prop(into, optional)] class: Signal<String>,
) -> impl IntoView {
    let total = if total > 0.0 { total } else { 100.0 };
    let percent = move || value.map(|v| (v.get() / total * 100.0).clamp(0.0, 100.0));
    let classes = move || {
        let variant = variant.map(|v| format!("progress--{}", v.get()));
        join_classes(&[
            "progress",
            variant.as_deref().unwrap_or(""),
            if large { "progress--lg" } else { "" },
            if value.is_none() {
                "progress--indeterminate"
            } else {
                ""
            },
            &class.get(),
        ])
    };
    view! {
        <div
            class=classes
            role="progressbar"
            aria-valuemin="0"
            aria-valuemax=total.to_string()
            aria-valuenow=move || value.map(|v| v.get().to_string())
            aria-label=label
        >
            <div class="progress__bar" style:width=move || percent().map(|p| format!("{p}%"))></div>
        </div>
    }
}

/// A transient notification card. Render inside `<div class="toast-region">` (fixed
/// bottom right); the region is a polite live region.
#[component]
pub fn Toast(
    /// `"success"`, `"danger"`, `"warning"` or none.
    #[prop(optional)]
    variant: Option<&'static str>,
    #[prop(optional)] icon: Option<&'static str>,
    #[prop(into, optional)] on_dismiss: Option<Callback<()>>,
    children: Children,
) -> impl IntoView {
    let class = variant.map_or_else(|| "toast".to_owned(), |v| format!("toast toast--{v}"));
    let icon = icon.or(match variant {
        Some("success") => Some("circle-check"),
        Some("danger") => Some("circle-alert"),
        Some("warning") => Some("triangle-alert"),
        _ => None,
    });
    view! {
        <div class=class>
            {icon.map(|name| view! { <Icon name=name/> })}
            <div class="grow">{children()}</div>
            {on_dismiss.map(|cb| view! {
                <button type="button" class="btn btn--ghost btn--icon btn--xs" aria-label="Dismiss"
                    on:click=move |_| cb.run(())><Icon name="x"/></button>
            })}
        </div>
    }
}

/// A colored dot: `online` (green), `away` (amber), `busy` (amber, no halo), `offline`
/// (red) or anything else (grey). `label` is the accessible name.
#[component]
pub fn StatusDot(
    #[prop(into)] status: Signal<&'static str>,
    #[prop(into, optional)] label: Option<Signal<String>>,
    #[prop(into, optional)] pulse: Signal<bool>,
) -> impl IntoView {
    let class = move || {
        let mut class = format!("status-dot status-dot--{}", status.get());
        if pulse.get() {
            class.push_str(" status-dot--pulse");
        }
        class
    };
    let has_label = label.is_some();
    view! {
        <span
            class=class
            role=has_label.then_some("img")
            aria-hidden=(!has_label).then_some("true")
            aria-label=move || label.map(|l| l.get())
            title=move || label.map(|l| l.get())
        ></span>
    }
}

/// A round avatar with the first letters of `name` (or an image).
#[component]
pub fn Avatar(
    #[prop(into)] name: Signal<String>,
    #[prop(into, optional)] src: Option<Signal<String>>,
    /// `"sm"` or `"lg"`.
    #[prop(optional)]
    size: Option<&'static str>,
) -> impl IntoView {
    let class = size.map_or_else(|| "avatar".to_owned(), |s| format!("avatar avatar--{s}"));
    let initials = move || {
        name.with(|n| {
            n.split(|c: char| c.is_whitespace() || c == '_' || c == '-')
                .filter_map(|part| part.chars().next())
                .take(2)
                .collect::<String>()
        })
    };
    view! {
        <span class=class aria-hidden="true">
            {move || match src.map(|s| s.get()).filter(|s| !s.is_empty()) {
                Some(src) => view! { <img src=src alt=""/> }.into_any(),
                None => initials().into_any(),
            }}
        </span>
    }
}
