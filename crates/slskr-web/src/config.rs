//! Runtime configuration: the `web.url_base` the daemon injects into `index.html`.

use std::cell::OnceCell;

thread_local! {
    static URL_BASE: OnceCell<String> = const { OnceCell::new() };
}

/// Reads `window.urlBase` (injected by the daemon into index.html); `"/"` and missing values map to `""`.
fn read_url_base() -> String {
    let Some(window) = web_sys::window() else {
        return String::new();
    };
    js_sys::Reflect::get(&window, &"urlBase".into())
        .ok()
        .and_then(|value| value.as_string())
        .filter(|base| base != "/")
        .unwrap_or_default()
}

/// The URL base the UI is served under (e.g. `""` or `"/slskr"`).
pub fn url_base() -> String {
    URL_BASE.with(|cell| cell.get_or_init(read_url_base).clone())
}

/// Root URL of the daemon (same as [`url_base`] in production builds).
pub fn root_url() -> String {
    url_base()
}

/// Base URL of the HTTP API (`{url_base}/api`).
pub fn api_base_url() -> String {
    format!("{}/api", root_url())
}

/// Path of the live updates WebSocket (`{url_base}/api/events`).
pub fn events_path() -> String {
    format!("{}{}", root_url(), slskr_types::realtime::PATH)
}
