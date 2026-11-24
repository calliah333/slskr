//! slskr's design system components (styled by `assets/static/css/app.css`; reference in
//! `docs/UI.md`). They carry no business logic.
//!
//! Many components are offered ahead of their first use while pages move over to them.
#![allow(dead_code)]

mod brand;
mod button;
mod card;
mod context_menu;
mod controls;
mod feedback;
pub mod form;
mod icon;
mod icon_data;
mod modal;
pub mod selection;
pub mod theme;

// Not every component is used yet.
#[allow(unused_imports)]
pub use self::{
    brand::*,
    button::*,
    card::*,
    context_menu::*,
    controls::*,
    feedback::*,
    icon::*,
    modal::*,
    selection::{ListAction, ListSelection},
    theme::ThemeToggle,
};

/// Joins non-empty class fragments with spaces.
pub(crate) fn join_classes(parts: &[&str]) -> String {
    parts
        .iter()
        .filter(|part| !part.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join(" ")
}
