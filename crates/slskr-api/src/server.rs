//! Soulseek server connection state (`GET /api/v0/server`, `State.server`).

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Soulseek client state flags (Soulseek.NET `SoulseekClientStates`), serialized like .NET's
/// `JsonStringEnumConverter` does for `[Flags]` enums: `"Connected, LoggedIn"`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct SoulseekClientStates(pub u32);

impl SoulseekClientStates {
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

    /// Raw bit value.
    pub const fn bits(self) -> u32 {
        self.0
    }

    /// Whether all bits of `other` are set (`Enum.HasFlag`).
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Parses a .NET flags string (`"Connected, LoggedIn"`, case-insensitive) or a numeric string.
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Ok(n) = s.parse::<u32>() {
            return Some(Self(n));
        }
        let mut bits = 0;
        for part in s.split(',') {
            let part = part.trim();
            if part.eq_ignore_ascii_case("None") {
                continue;
            }
            let (value, _) = Self::NAMES
                .iter()
                .find(|(_, name)| name.eq_ignore_ascii_case(part))?;
            bits |= value;
        }
        Some(Self(bits))
    }
}

impl std::ops::BitOr for SoulseekClientStates {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        Self(self.0 | rhs.0)
    }
}

impl std::ops::BitOrAssign for SoulseekClientStates {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

impl fmt::Display for SoulseekClientStates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("None");
        }
        let known = Self::NAMES.iter().fold(0, |acc, (v, _)| acc | v);
        if self.0 & !known != 0 {
            // .NET emits the number when the value has undefined bits
            return write!(f, "{}", self.0);
        }
        let mut first = true;
        for (value, name) in Self::NAMES {
            if self.0 & value != 0 {
                if !first {
                    f.write_str(", ")?;
                }
                f.write_str(name)?;
                first = false;
            }
        }
        Ok(())
    }
}

impl Serialize for SoulseekClientStates {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for SoulseekClientStates {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct V;
        impl Visitor<'_> for V {
            type Value = SoulseekClientStates;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("SoulseekClientStates flags string or integer")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Self::Value, E> {
                SoulseekClientStates::parse(v)
                    .ok_or_else(|| E::custom(format!("invalid SoulseekClientStates '{v}'")))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> Result<Self::Value, E> {
                u32::try_from(v)
                    .map(SoulseekClientStates)
                    .map_err(E::custom)
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> Result<Self::Value, E> {
                u32::try_from(v)
                    .map(SoulseekClientStates)
                    .map_err(E::custom)
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// Server connection state (slskd `ServerState` record in `State.cs`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ServerState {
    pub address: Option<String>,
    /// `IPEndPoint.ToString()`, e.g. `"1.2.3.4:2271"`.
    #[serde(rename = "ipEndPoint")]
    pub ip_end_point: Option<String>,
    pub state: SoulseekClientStates,
    pub is_connected: bool,
    pub is_connecting: bool,
    pub is_logged_in: bool,
    pub is_logging_in: bool,
    pub is_transitioning: bool,
}

impl ServerState {
    /// Builds the state, deriving the computed `is*` properties from `state` as slskd does.
    pub fn new(
        address: Option<String>,
        ip_end_point: Option<String>,
        state: SoulseekClientStates,
    ) -> Self {
        use SoulseekClientStates as S;
        Self {
            address,
            ip_end_point,
            state,
            is_connected: state.contains(S::CONNECTED),
            is_connecting: state.contains(S::CONNECTING),
            is_logged_in: state.contains(S::LOGGED_IN),
            is_logging_in: state.contains(S::LOGGING_IN),
            is_transitioning: state.contains(S::CONNECTING)
                || state.contains(S::DISCONNECTING)
                || state.contains(S::LOGGING_IN),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_roundtrip() {
        let s = SoulseekClientStates::CONNECTED | SoulseekClientStates::LOGGED_IN;
        assert_eq!(serde_json::to_value(s).unwrap(), "Connected, LoggedIn");
        assert_eq!(
            serde_json::to_value(SoulseekClientStates::NONE).unwrap(),
            "None"
        );
        let back: SoulseekClientStates = serde_json::from_str("\"Connected, LoggedIn\"").unwrap();
        assert_eq!(back, s);
        let num: SoulseekClientStates = serde_json::from_str("6").unwrap();
        assert_eq!(num, s);
    }

    #[test]
    fn server_state_fields() {
        let s = ServerState::new(
            Some("vps.slsknet.org:2271".into()),
            Some("1.2.3.4:2271".into()),
            SoulseekClientStates(6),
        );
        let v = serde_json::to_value(&s).unwrap();
        assert_eq!(v["ipEndPoint"], "1.2.3.4:2271");
        assert_eq!(v["isLoggedIn"], true);
        assert_eq!(v["isTransitioning"], false);
    }
}
