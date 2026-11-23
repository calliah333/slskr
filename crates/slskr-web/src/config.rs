//! Runtime configuration (port of slskd `src/config.js`).

use std::cell::OnceCell;

/// Storage key holding the active chat conversation.
pub const ACTIVE_CHAT_KEY: &str = "slskd-active-chat";
/// Storage key holding the active room.
pub const ACTIVE_ROOM_KEY: &str = "slskd-active-room";
/// Storage key holding the active user info tab.
pub const ACTIVE_USER_INFO_KEY: &str = "slskd-active-user";

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

/// The URL base the UI is served under (e.g. `""` or `"/slskd"`).
pub fn url_base() -> String {
    URL_BASE.with(|cell| cell.get_or_init(read_url_base).clone())
}

/// Root URL of the daemon (same as [`url_base`] in production builds).
pub fn root_url() -> String {
    url_base()
}

/// Base URL of the HTTP API (`{url_base}/api/v0`).
pub fn api_base_url() -> String {
    format!("{}/api/v0", root_url())
}

/// Path of the live updates WebSocket (`{url_base}/api/events`).
pub fn events_path() -> String {
    format!("{}{}", root_url(), slskr_api::realtime::PATH)
}
