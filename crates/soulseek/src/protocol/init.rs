//! Peer connection initialization messages.

use bytes::Bytes;

use super::codec::{MessageBuilder, MessageReader, ProtocolError};
use super::codes::InitCode;

/// Announces the sender's username, connection type (`"P"`, `"F"`, `"D"`) and token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInit {
    pub username: String,
    pub connection_type: String,
    pub token: u32,
}

impl PeerInit {
    pub fn to_bytes(&self) -> Bytes {
        MessageBuilder::new()
            .code_u8(InitCode::PeerInit.as_u8())
            .string(&self.username)
            .string(&self.connection_type)
            .u32(self.token)
            .build()
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        Ok(Self {
            username: r.string()?,
            connection_type: r.string()?,
            token: r.u32()?,
        })
    }
}

/// Answers a solicited (indirect) connection with its token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PierceFirewall {
    pub token: u32,
}

impl PierceFirewall {
    pub fn to_bytes(&self) -> Bytes {
        MessageBuilder::new()
            .code_u8(InitCode::PierceFirewall.as_u8())
            .u32(self.token)
            .build()
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        Ok(Self { token: r.u32()? })
    }
}

/// Incoming initialization message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InitIncoming {
    PierceFirewall(PierceFirewall),
    PeerInit(PeerInit),
    Unknown { code: u8, body: Bytes },
}

impl InitIncoming {
    pub fn decode(body: Bytes) -> Result<Self, ProtocolError> {
        let mut r = MessageReader::new(body.clone());
        let code = r.code_u8()?;
        Ok(match InitCode::try_from(code) {
            Ok(InitCode::PierceFirewall) => Self::PierceFirewall(PierceFirewall::decode(&mut r)?),
            Ok(InitCode::PeerInit) => Self::PeerInit(PeerInit::decode(&mut r)?),
            Err(_) => Self::Unknown { code, body },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(frame: Bytes) -> Bytes {
        frame.slice(4..)
    }

    #[test]
    fn peer_init_bytes() {
        let m = PeerInit {
            username: "foo".into(),
            connection_type: "P".into(),
            token: 42,
        };
        let b = m.to_bytes();
        assert_eq!(
            &b[..],
            &[
                17, 0, 0, 0, 1, 3, 0, 0, 0, b'f', b'o', b'o', 1, 0, 0, 0, b'P', 42, 0, 0, 0
            ]
        );
        assert_eq!(
            InitIncoming::decode(body(b)).unwrap(),
            InitIncoming::PeerInit(m)
        );
    }

    #[test]
    fn pierce_firewall_bytes() {
        let m = PierceFirewall { token: 0x0102_0304 };
        let b = m.to_bytes();
        assert_eq!(&b[..], &[5, 0, 0, 0, 0, 4, 3, 2, 1]);
        assert_eq!(
            InitIncoming::decode(body(b)).unwrap(),
            InitIncoming::PierceFirewall(m)
        );
    }

    #[test]
    fn unknown_and_truncated() {
        let b = Bytes::from_static(&[7, 1, 2]);
        assert_eq!(
            InitIncoming::decode(b.clone()).unwrap(),
            InitIncoming::Unknown { code: 7, body: b }
        );
        assert!(InitIncoming::decode(Bytes::from_static(&[0, 1])).is_err());
        assert!(InitIncoming::decode(Bytes::new()).is_err());
    }
}
