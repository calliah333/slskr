//! `Pager`: page links below a paged list.

use leptos::prelude::*;

use super::icon::Icon;

/// The page numbers to show around `current` (0-based) of `pages`: the first, the last,
/// and two either side of the current one; `None` is a gap.
fn window(current: u32, pages: u32) -> Vec<Option<u32>> {
    let mut shown = Vec::new();
    for page in 0..pages {
        let near = page.abs_diff(current) <= 2;
        if page == 0 || page + 1 == pages || near {
            shown.push(Some(page));
        } else if shown.last().is_some_and(Option::is_some) {
            shown.push(None);
        }
    }
    shown
}

/// Previous and next buttons with the page numbers between: `<nav class="pager">`. `page`
/// is 0-based; nothing is rendered for a single page.
#[component]
pub fn Pager(
    page: RwSignal<u32>,
    #[prop(into)] pages: Signal<u32>,
    /// What the pages hold, for the accessible name ("Upload pages").
    #[prop(into)]
    label: String,
) -> impl IntoView {
    let go = move |to: u32| {
        if to != page.get_untracked() {
            page.set(to);
        }
    };
    view! {
        <Show when=move || { pages.get() > 1 }>
            <nav class="pager" aria-label=label.clone()>
                <button type="button" class="btn btn--ghost btn--sm btn--icon" aria-label="Previous page"
                    title="Previous page" disabled=move || page.get() == 0
                    on:click=move |_| go(page.get_untracked().saturating_sub(1))>
                    <Icon name="chevron-left"/>
                </button>
                {move || window(page.get(), pages.get()).into_iter().map(|item| match item {
                    Some(number) => view! {
                        <button type="button" class="btn btn--ghost btn--sm pager__page tabular"
                            aria-current=move || (page.get() == number).then_some("page")
                            on:click=move |_| go(number)>
                            {number + 1}
                        </button>
                    }.into_any(),
                    None => view! { <span class="pager__gap muted" aria-hidden="true">"…"</span> }.into_any(),
                }).collect_view()}
                <button type="button" class="btn btn--ghost btn--sm btn--icon" aria-label="Next page"
                    title="Next page" disabled=move || { page.get() + 1 >= pages.get() }
                    on:click=move |_| go(page.get_untracked() + 1)>
                    <Icon name="chevron-right"/>
                </button>
            </nav>
        </Show>
    }
}

#[cfg(test)]
mod tests {
    use super::window;

    #[test]
    fn shows_the_ends_and_the_pages_around_the_current_one() {
        assert_eq!(window(0, 1), [Some(0)]);
        assert_eq!(window(0, 4), [Some(0), Some(1), Some(2), Some(3)]);
        assert_eq!(
            window(5, 20),
            [
                Some(0),
                None,
                Some(3),
                Some(4),
                Some(5),
                Some(6),
                Some(7),
                None,
                Some(19)
            ]
        );
    }
}
