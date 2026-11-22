//! Client state, distributed network info and peer connection types.

use std::fmt;
use std::net::SocketAddr;
use std::ops::{BitAnd, BitOr, BitOrAssign};

use super::transfer::{flags_from_str, flags_to_string};

/// Client state flags (Soulseek.NET `SoulseekClientStates`).
///
/// A fully operational client is `CONNECTED | LOGGED_IN`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ClientState(pub u32);

impl ClientState {
    pub const NONE: Self = Self(0);
    pub const DISCONNECTED: Self = Self(1);
    pub const CONNECTED: Self = Self(2);
    pub const LOGGED_IN: Self = Self(4);
    pub const CONNECTING: Self = Self(8);
    pub const LOGGING_IN: Self = Self(16);
    pub const DISCONNECTING: Self = Self(32);

    const NAMES: [(u32, &'static str); 6] = [
        (1, "Disconnected"),
        (2, "Connected"),
        (4, "LoggedIn"),
        (8, "Connecting"),
        (16, "LoggingIn"),
        (32, "Disconnecting"),
    ];

    /// Raw bits.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether all flags of `other` are set.
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether any flag of `other` is set.
    pub const fn intersects(self, other: Self) -> bool {
        self.0 & other.0 != 0
    }

    /// Whether the client is `CONNECTED | LOGGED_IN`.
    pub const fn is_logged_in(self) -> bool {
        self.contains(Self(Self::CONNECTED.0 | Self::LOGGED_IN.0))
    }

    /// .NET `[Flags]` `ToString()`, e.g. `"Connected, LoggedIn"`.
    pub fn to_slskd_string(self) -> String {
        flags_to_string(self.0, &Self::NAMES)
    }

    /// Parses the output of [`Self::to_slskd_string`].
    pub fn from_slskd_str(s: &str) -> Option<Self> {
        flags_from_str(s, &Self::NAMES).map(Self)
    }
}

impl fmt::Display for ClientState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_slskd_string())
    }
}

impl BitOr for ClientState {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for ClientState {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for ClientState {
    type Output = Self;
    fn bitand(self, rhs: Self) -> Self {
        Self(self.0 & rhs.0)
    }
}

/// A peer on the distributed network (parent or child).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DistributedPeer {
    pub username: String,
    pub ip_endpoint: SocketAddr,
}

/// Snapshot of the distributed network state (Soulseek.NET `DistributedNetworkInfo`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DistributedNetworkInfo {
    /// Average child broadcast latency in milliseconds, once measured.
    pub average_broadcast_latency: Option<f64>,
    pub branch_level: u32,
    /// Username of the branch root (our own username when we are the root).
    pub branch_root: String,
    pub is_branch_root: bool,
    pub child_limit: u32,
    pub can_accept_children: bool,
    pub children: Vec<DistributedPeer>,
    pub parent: Option<DistributedPeer>,
    pub has_parent: bool,
}

/// Kind of a peer connection, sent as a one-letter string in init messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConnectionType {
    /// `"P"`: peer messages.
    Peer,
    /// `"F"`: file transfer.
    Transfer,
    /// `"D"`: distributed network.
    Distributed,
}

impl ConnectionType {
    /// Wire string (`"P"`, `"F"`, `"D"`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Peer => "P",
            Self::Transfer => "F",
            Self::Distributed => "D",
        }
    }

    /// Wire character (`'P'`, `'F'`, `'D'`).
    pub fn as_char(self) -> char {
        match self {
            Self::Peer => 'P',
            Self::Transfer => 'F',
            Self::Distributed => 'D',
        }
    }

    /// Parses a wire string.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "P" => Some(Self::Peer),
            "F" => Some(Self::Transfer),
            "D" => Some(Self::Distributed),
            _ => None,
        }
    }
}

impl fmt::Display for ConnectionType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_strings() {
        assert_eq!(
            (ClientState::LOGGED_IN | ClientState::CONNECTED).to_slskd_string(),
            "Connected, LoggedIn"
        );
        assert_eq!(ClientState::DISCONNECTED.to_slskd_string(), "Disconnected");
        assert!((ClientState::CONNECTED | ClientState::LOGGED_IN).is_logged_in());
        assert!(!ClientState::CONNECTED.is_logged_in());
        assert_eq!(
            ClientState::from_slskd_str("Connected, LoggingIn"),
            Some(ClientState::CONNECTED | ClientState::LOGGING_IN)
        );
    }

    #[test]
    fn connection_type_wire() {
        for t in [
            ConnectionType::Peer,
            ConnectionType::Transfer,
            ConnectionType::Distributed,
        ] {
            assert_eq!(ConnectionType::from_wire(t.as_str()), Some(t));
        }
        assert_eq!(ConnectionType::from_wire("X"), None);
    }
}
