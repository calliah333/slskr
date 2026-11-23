//! `Icon`: inline SVG line icons from a curated set of [lucide](https://lucide.dev) icons
//! (ISC license, `licenses/lucide-LICENSE`). The markup lives in `icon_data.rs`.
//!
//! Names are lucide's (`"folder-open"`, `"trash-2"`); a few common aliases are accepted
//! (see [`ALIASES`]). An unknown name renders `circle-help` and logs a warning in debug
//! builds. To add an icon, append its lucide name to the list in the generator noted at
//! the top of `icon_data.rs`, or copy the inner markup of `lucide-static/icons/<name>.svg`
//! into `ICONS` keeping it sorted.
//!
//! Semantic UI icon names the old UI used, and their lucide equivalents:
//!
//! | Semantic UI                      | lucide            |
//! |----------------------------------|-------------------|
//! | search                           | search            |
//! | folder, folder open              | folder, folder-open |
//! | x, close, delete                 | x                 |
//! | trash alternate                  | trash-2           |
//! | redo / refresh / sync (alternate)| rotate-cw / refresh-cw |
//! | lock                             | lock (unlock: lock-open) |
//! | download, upload                 | download, upload  |
//! | chevron right / down             | chevron-right / chevron-down |
//! | plug (connected/disconnected)    | plug / unplug     |
//! | flask (canary)                   | flask-conical     |
//! | exclamation circle               | circle-alert      |
//! | clock, time                      | clock             |
//! | circle (status)                  | use `StatusDot`   |
//! | bullhorn (new version)           | megaphone         |
//! | users, user                      | users, user       |
//! | stop circle                      | circle-stop       |
//! | info circle                      | info              |
//! | sign in, sign out                | log-in, log-out   |
//! | plus                             | plus              |
//! | level up alternate               | corner-left-up    |
//! | left arrow                       | arrow-left        |
//! | loading circle notch             | loader-circle (with `spin`) |
//! | check                            | check             |
//! | arrow up / down                  | arrow-up / arrow-down |
//! | filter                           | filter            |
//! | copy                             | copy              |
//! | comments (rooms), comment (chat) | messages-square, message-square |
//! | cogs (system)                    | settings          |
//! | chart bar (dashboard)            | chart-column      |
//! | adjust (theme)                   | sun / moon / monitor (`ThemeToggle`) |

use leptos::prelude::*;

use super::icon_data::ICONS;

/// Alternative names accepted by [`Icon`] (`alias -> lucide name`).
pub const ALIASES: &[(&str, &str)] = &[
    ("alert-circle", "circle-alert"),
    ("alert-triangle", "triangle-alert"),
    ("bar-chart", "chart-column"),
    ("help", "circle-help"),
    ("loader", "loader-circle"),
    ("messages", "messages-square"),
    ("more-horizontal", "ellipsis"),
    ("sort", "arrow-up-down"),
    ("stop", "square"),
    ("trash", "trash-2"),
    ("unlock", "lock-open"),
    ("warning", "triangle-alert"),
];

/// The inner SVG markup of a named icon, if known (lucide names or [`ALIASES`]).
pub fn icon_markup(name: &str) -> Option<&'static str> {
    let name = ALIASES
        .iter()
        .find(|(alias, _)| *alias == name)
        .map_or(name, |(_, real)| real);
    ICONS
        .binary_search_by(|(known, _)| (*known).cmp(name))
        .ok()
        .map(|index| ICONS[index].1)
}

/// All icon names (lucide names, sorted), e.g. for documentation.
pub fn icon_names() -> impl Iterator<Item = &'static str> {
    ICONS.iter().map(|(name, _)| *name)
}

/// An inline SVG icon: `<svg class="icon icon--{size}">`, `1rem` square by default and
/// drawn in `currentColor`.
///
/// Decorative (`aria-hidden`) unless `label` is given, which makes it an `img` with that
/// accessible name. Icon-only buttons should put the label on the button instead.
#[component]
pub fn Icon(
    /// lucide icon name, e.g. `"folder-open"`.
    #[prop(into)]
    name: Signal<String>,
    /// `"xs"` (12px), `"sm"` (14px), `"lg"` (20px), `"xl"` (24px) or `"2xl"` (32px).
    #[prop(optional)]
    size: Option<&'static str>,
    /// Rotate continuously (loaders).
    #[prop(into, optional)]
    spin: Signal<bool>,
    /// Extra classes, e.g. `text-success`.
    #[prop(into, optional)]
    class: Signal<String>,
    /// Accessible name; without it the icon is hidden from assistive technology.
    #[prop(into, optional)]
    label: Option<String>,
) -> impl IntoView {
    let classes = move || {
        let mut classes = String::from("icon");
        if let Some(size) = size {
            classes.push_str(" icon--");
            classes.push_str(size);
        }
        if spin.get() {
            classes.push_str(" icon--spin");
        }
        class.with(|extra| {
            if !extra.is_empty() {
                classes.push(' ');
                classes.push_str(extra);
            }
        });
        classes
    };
    let markup = move || {
        name.with(|name| {
            icon_markup(name).unwrap_or_else(|| {
                #[cfg(debug_assertions)]
                leptos::logging::warn!("unknown icon {name:?}");
                icon_markup("circle-help").unwrap_or_default()
            })
        })
    };
    let hidden = label.is_none().then_some("true");
    let role = label.is_some().then_some("img");
    view! {
        <svg
            class=classes
            xmlns="http://www.w3.org/2000/svg"
            viewBox="0 0 24 24"
            fill="none"
            stroke="currentColor"
            stroke-width="2"
            stroke-linecap="round"
            stroke-linejoin="round"
            aria-hidden=hidden
            role=role
            aria-label=label
            focusable="false"
            inner_html=markup
        ></svg>
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icons_are_sorted_and_aliases_resolve() {
        assert!(ICONS.windows(2).all(|pair| pair[0].0 < pair[1].0));
        for (alias, real) in ALIASES {
            assert!(icon_markup(real).is_some(), "{alias} -> {real}");
        }
        assert!(icon_markup("folder-open").is_some());
        assert!(icon_markup("no-such-icon").is_none());
    }
}
