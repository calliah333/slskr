//! Light/dark theme: the `.dark` class on `<html>`, chosen as light, dark or system.
//!
//! `index.html` applies the stored choice before the first paint (no flash); [`install`]
//! takes over from there and follows `prefers-color-scheme` live while the choice is
//! `system`. The choice is kept per browser in `localStorage` under [`THEME_KEY`]; when
//! storage is blocked it lasts for the page only.

use leptos::prelude::*;
use wasm_bindgen::{JsCast, closure::Closure};

use super::icon::Icon;

/// `localStorage` key; the boot script in `index.html` reads the same key.
pub const THEME_KEY: &str = "slskr-theme";

/// The user's theme choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeChoice {
    Light,
    Dark,
    #[default]
    System,
}

impl ThemeChoice {
    fn parse(value: Option<&str>) -> Self {
        match value {
            Some("light") => Self::Light,
            Some("dark") => Self::Dark,
            _ => Self::System,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Light => "light",
            Self::Dark => "dark",
            Self::System => "system",
        }
    }

    /// light -> dark -> system -> light.
    pub fn next(self) -> Self {
        match self {
            Self::Light => Self::Dark,
            Self::Dark => Self::System,
            Self::System => Self::Light,
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::Light => "sun",
            Self::Dark => "moon",
            Self::System => "monitor",
        }
    }
}

/// The theme state, provided as context by [`install`].
#[derive(Clone, Copy)]
pub struct Theme {
    pub choice: RwSignal<ThemeChoice>,
}

fn storage() -> Option<web_sys::Storage> {
    window().local_storage().ok().flatten()
}

fn system_query() -> Option<web_sys::MediaQueryList> {
    window()
        .match_media("(prefers-color-scheme: dark)")
        .ok()
        .flatten()
}

/// Reads the stored choice, keeps `<html class="dark">` in step and provides [`Theme`].
pub fn install() -> Theme {
    let stored = storage().and_then(|s| s.get_item(THEME_KEY).ok().flatten());
    let choice = RwSignal::new(ThemeChoice::parse(stored.as_deref()));
    let system = system_query();
    let system_dark = RwSignal::new(
        system
            .as_ref()
            .is_some_and(web_sys::MediaQueryList::matches),
    );
    if let Some(system) = system {
        let media = system.clone();
        let listener = Closure::<dyn FnMut(web_sys::Event)>::new(move |_| {
            system_dark.try_set(media.matches());
        });
        if system
            .add_event_listener_with_callback("change", listener.as_ref().unchecked_ref())
            .is_ok()
        {
            let listener = StoredValue::new_local((system, listener));
            on_cleanup(move || {
                listener.with_value(|(system, listener)| {
                    let _ = system.remove_event_listener_with_callback(
                        "change",
                        listener.as_ref().unchecked_ref(),
                    );
                })
            });
        }
    }
    Effect::new(move |_| {
        let is_dark = match choice.get() {
            ThemeChoice::Light => false,
            ThemeChoice::Dark => true,
            ThemeChoice::System => system_dark.get(),
        };
        if let Some(root) = document().document_element() {
            let _ = root.class_list().toggle_with_force("dark", is_dark);
        }
    });
    let theme = Theme { choice };
    provide_context(theme);
    theme
}

/// Square icon button cycling light -> dark -> system (sun, moon, monitor icons).
#[component]
pub fn ThemeToggle() -> impl IntoView {
    let Theme { choice } = expect_context::<Theme>();
    let label = move || format!("Theme: {} (click to change)", choice.get().as_str());
    let cycle = move |_| {
        let next = choice.get_untracked().next();
        if let Some(storage) = storage() {
            let _ = storage.set_item(THEME_KEY, next.as_str());
        }
        choice.set(next);
    };
    view! {
        <button type="button" class="btn btn--icon theme-toggle" aria-label=label title=label
            data-theme=move || choice.get().as_str() on:click=cycle>
            <Icon name=Signal::derive(move || choice.get().icon().to_owned())/>
        </button>
    }
}
