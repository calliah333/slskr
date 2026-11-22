//! `Table` and its sub-components.

use leptos::ev::MouseEvent;
use leptos::prelude::*;

use super::classes::{Classes, KeyOrValue, Width};
use super::icon::Icon;

/// semantic-ui-react `Table`: `<table class="ui ... table">`.
#[component]
pub fn Table(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(into, optional)] attached: KeyOrValue,
    /// `true` or `"very"`.
    #[prop(into, optional)]
    basic: KeyOrValue,
    #[prop(optional)] celled: bool,
    #[prop(optional)] collapsing: bool,
    #[prop(optional)] color: Option<&'static str>,
    #[prop(into, optional)] columns: Option<Width>,
    /// `true` or `"very"`.
    #[prop(into, optional)]
    compact: KeyOrValue,
    #[prop(optional)] definition: bool,
    #[prop(optional)] fixed: bool,
    #[prop(optional)] inverted: bool,
    #[prop(into, optional)] padded: KeyOrValue,
    #[prop(optional)] selectable: bool,
    #[prop(optional)] single_line: bool,
    #[prop(optional)] size: Option<&'static str>,
    #[prop(optional)] sortable: bool,
    #[prop(optional)] stackable: bool,
    #[prop(optional)] striped: bool,
    #[prop(optional)] structured: bool,
    #[prop(optional)] text_align: Option<&'static str>,
    #[prop(optional)] unstackable: bool,
    #[prop(optional)] vertical_align: Option<&'static str>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        Classes::new("ui")
            .opt(color)
            .opt(size)
            .key(celled, "celled")
            .key(collapsing, "collapsing")
            .key(definition, "definition")
            .key(fixed, "fixed")
            .key(inverted, "inverted")
            .key(selectable, "selectable")
            .key(single_line, "single line")
            .key(sortable, "sortable")
            .key(stackable, "stackable")
            .key(striped, "striped")
            .key(structured, "structured")
            .key(unstackable, "unstackable")
            .key_or_value(attached, "attached")
            .key_or_value(basic, "basic")
            .key_or_value(compact, "compact")
            .key_or_value(padded, "padded")
            .text_align(text_align)
            .vertical_align(vertical_align)
            .width(columns, "column", false)
            .add("table")
            .add(&class.get())
            .build()
    };
    view! { <table class=classes>{children.map(|c| c())}</table> }
}

/// Class attribute value: `None` when empty so no `class=""` is emitted (React omits it).
fn class_attr(classes: String) -> Option<String> {
    (!classes.is_empty()).then_some(classes)
}

/// `Table.Header`: `<thead>`.
#[component]
pub fn TableHeader(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] full_width: bool,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        class_attr(
            Classes::new("")
                .key(full_width, "full-width")
                .add(&class.get())
                .build(),
        )
    };
    view! { <thead class=classes>{children.map(|c| c())}</thead> }
}

/// `Table.Body`: `<tbody>`.
#[component]
pub fn TableBody(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || class_attr(class.get());
    view! { <tbody class=classes>{children.map(|c| c())}</tbody> }
}

/// `Table.Footer`: `<tfoot>`.
#[component]
pub fn TableFooter(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] full_width: bool,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        class_attr(
            Classes::new("")
                .key(full_width, "full-width")
                .add(&class.get())
                .build(),
        )
    };
    view! { <tfoot class=classes>{children.map(|c| c())}</tfoot> }
}

/// `Table.Row`: `<tr>`.
#[component]
pub fn TableRow(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(into, optional)] active: Signal<bool>,
    #[prop(into, optional)] disabled: Signal<bool>,
    #[prop(into, optional)] error: Signal<bool>,
    #[prop(into, optional)] negative: Signal<bool>,
    #[prop(into, optional)] positive: Signal<bool>,
    #[prop(into, optional)] warning: Signal<bool>,
    #[prop(optional)] text_align: Option<&'static str>,
    #[prop(optional)] vertical_align: Option<&'static str>,
    #[prop(into, optional)] on_click: Option<Callback<MouseEvent>>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        class_attr(
            Classes::new("")
                .key(active.get(), "active")
                .key(disabled.get(), "disabled")
                .key(error.get(), "error")
                .key(negative.get(), "negative")
                .key(positive.get(), "positive")
                .key(warning.get(), "warning")
                .text_align(text_align)
                .vertical_align(vertical_align)
                .add(&class.get())
                .build(),
        )
    };
    let handler = move |ev: MouseEvent| {
        if let Some(cb) = on_click {
            cb.run(ev);
        }
    };
    view! { <tr class=classes on:click=handler>{children.map(|c| c())}</tr> }
}

/// Builds the shared `Table.Cell` / `Table.HeaderCell` class string.
#[allow(clippy::too_many_arguments)]
fn cell_classes(
    active: bool,
    collapsing: bool,
    disabled: bool,
    error: bool,
    negative: bool,
    positive: bool,
    selectable: bool,
    single_line: bool,
    warning: bool,
    text_align: Option<&str>,
    vertical_align: Option<&str>,
    width: Option<Width>,
    extra: Classes,
) -> Option<String> {
    let mut c = Classes::new("")
        .key(active, "active")
        .key(collapsing, "collapsing")
        .key(disabled, "disabled")
        .key(error, "error")
        .key(negative, "negative")
        .key(positive, "positive")
        .key(selectable, "selectable")
        .key(single_line, "single line")
        .key(warning, "warning")
        .text_align(text_align)
        .vertical_align(vertical_align)
        .width(width, "wide", false);
    c = c.add(&extra.build());
    class_attr(c.build())
}

/// `Table.Cell`: `<td>`. `icon` renders an icon shorthand before the children.
#[component]
pub fn TableCell(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] active: bool,
    #[prop(optional)] collapsing: bool,
    #[prop(optional)] disabled: bool,
    #[prop(optional)] error: bool,
    #[prop(optional)] icon: Option<&'static str>,
    #[prop(optional)] negative: bool,
    #[prop(optional)] positive: bool,
    #[prop(optional)] selectable: bool,
    #[prop(optional)] single_line: bool,
    #[prop(optional)] text_align: Option<&'static str>,
    #[prop(optional)] vertical_align: Option<&'static str>,
    #[prop(optional)] warning: bool,
    #[prop(into, optional)] width: Option<Width>,
    #[prop(optional)] col_span: Option<u32>,
    #[prop(into, optional)] on_click: Option<Callback<MouseEvent>>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        cell_classes(
            active,
            collapsing,
            disabled,
            error,
            negative,
            positive,
            selectable,
            single_line,
            warning,
            text_align,
            vertical_align,
            width,
            Classes::new(&class.get()),
        )
    };
    let handler = move |ev: MouseEvent| {
        if let Some(cb) = on_click {
            cb.run(ev);
        }
    };
    view! {
        <td class=classes colspan=col_span on:click=handler>
            {icon.map(|name| view! { <Icon name=name /> })}
            {children.map(|c| c())}
        </td>
    }
}

/// `Table.HeaderCell`: `<th>`. `sorted` is `"ascending"` / `"descending"`.
#[component]
pub fn TableHeaderCell(
    #[prop(into, optional)] class: Signal<String>,
    #[prop(optional)] collapsing: bool,
    #[prop(optional)] single_line: bool,
    #[prop(into, optional)] sorted: Signal<Option<String>>,
    #[prop(optional)] text_align: Option<&'static str>,
    #[prop(optional)] vertical_align: Option<&'static str>,
    #[prop(into, optional)] width: Option<Width>,
    #[prop(optional)] col_span: Option<u32>,
    #[prop(into, optional)] on_click: Option<Callback<MouseEvent>>,
    #[prop(optional)] children: Option<Children>,
) -> impl IntoView {
    let classes = move || {
        cell_classes(
            false,
            collapsing,
            false,
            false,
            false,
            false,
            false,
            single_line,
            false,
            text_align,
            vertical_align,
            width,
            Classes::new("")
                .value_key(sorted.get().as_deref(), "sorted")
                .add(&class.get()),
        )
    };
    let handler = move |ev: MouseEvent| {
        if let Some(cb) = on_click {
            cb.run(ev);
        }
    };
    view! { <th class=classes colspan=col_span on:click=handler>{children.map(|c| c())}</th> }
}
