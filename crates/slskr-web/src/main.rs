//! slskr web UI: a client-side rendered Leptos app.

mod components;
mod config;
mod services;
mod ui;

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to(
        leptos::prelude::document()
            .get_element_by_id("root")
            .expect("#root element")
            .unchecked_into(),
        components::app::App,
    )
    .forget();
}

use wasm_bindgen::JsCast;
