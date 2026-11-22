//! Session token persistence (port of slskd `lib/token.js`).

use crate::config::{TOKEN_KEY, TOKEN_PASSTHROUGH_VALUE};

/// Browser storage area a token is written to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TokenStorage {
    /// `localStorage`: survives browser restarts ("remember me").
    Local,
    /// `sessionStorage`: cleared when the tab closes.
    Session,
}

impl TokenStorage {
    /// `Local` when `remember_me` is set, otherwise `Session` (as in slskd's login).
    pub fn for_remember_me(remember_me: bool) -> Self {
        if remember_me {
            Self::Local
        } else {
            Self::Session
        }
    }

    fn storage(self) -> Option<web_sys::Storage> {
        let window = web_sys::window()?;
        match self {
            Self::Local => window.local_storage().ok().flatten(),
            Self::Session => window.session_storage().ok().flatten(),
        }
    }
}

fn read(storage: TokenStorage) -> Option<String> {
    storage
        .storage()?
        .get_item(TOKEN_KEY)
        .ok()
        .flatten()
        .filter(|token| !token.is_empty())
}

/// Returns the stored token, preferring `sessionStorage` over `localStorage`.
pub fn get_token() -> Option<String> {
    read(TokenStorage::Session).or_else(|| read(TokenStorage::Local))
}

/// Stores `token` in the given storage area.
pub fn set_token(storage: TokenStorage, token: &str) {
    if let Some(storage) = storage.storage() {
        let _ = storage.set_item(TOKEN_KEY, token);
    }
}

/// Removes the token from both storage areas.
pub fn clear_token() {
    for storage in [TokenStorage::Local, TokenStorage::Session] {
        if let Some(storage) = storage.storage() {
            let _ = storage.remove_item(TOKEN_KEY);
        }
    }
}

/// True when the stored token is the passthrough marker (authentication disabled).
pub fn is_passthrough_enabled() -> bool {
    get_token().as_deref() == Some(TOKEN_PASSTHROUGH_VALUE)
}
