//! Distributed network messages (`u8` codes).

use bytes::{BufMut, Bytes, BytesMut};

use super::codec::{MessageBuilder, MessageReader, ProtocolError};
use super::codes::DistributedCode;

/// A search request travelling down the distributed tree.
///
/// The wire format carries an unexplained leading `u32` which is ignored on
/// read and written as `0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistributedSearchRequest {
    pub username: String,
    pub token: u32,
    pub query: String,
}

impl DistributedSearchRequest {
    pub fn to_bytes(&self) -> Bytes {
        MessageBuilder::new()
            .code_u8(DistributedCode::SearchRequest.as_u8())
            .u32(0)
            .string(&self.username)
            .u32(self.token)
            .string(&self.query)
            .build()
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        // nobody knows what this is.
        r.u32()?;
        Ok(Self {
            username: r.string()?,
            token: r.u32()?,
            query: r.string()?,
        })
    }
}

/// Ping sent by a parent to its children (code only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DistributedPingRequest;

impl DistributedPingRequest {
    pub fn to_bytes(&self) -> Bytes {
        MessageBuilder::new()
            .code_u8(DistributedCode::Ping.as_u8())
            .build()
    }

    pub fn decode(_r: &mut MessageReader) -> Result<Self, ProtocolError> {
        Ok(Self)
    }
}

/// Ping reply carrying a token; the token is optional on the wire and defaults to `0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DistributedPingResponse {
    pub token: u32,
}

impl DistributedPingResponse {
    pub fn to_bytes(&self) -> Bytes {
        MessageBuilder::new()
            .code_u8(DistributedCode::Ping.as_u8())
            .u32(self.token)
            .build()
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let token = if r.has_remaining() { r.u32()? } else { 0 };
        Ok(Self { token })
    }
}

/// The sender's distance from its branch root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DistributedBranchLevel {
    pub level: i32,
}

impl DistributedBranchLevel {
    pub fn to_bytes(&self) -> Bytes {
        MessageBuilder::new()
            .code_u8(DistributedCode::BranchLevel.as_u8())
            .i32(self.level)
            .build()
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        Ok(Self { level: r.i32()? })
    }
}

/// The username of the sender's branch root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DistributedBranchRoot {
    pub username: String,
}

impl DistributedBranchRoot {
    pub fn to_bytes(&self) -> Bytes {
        MessageBuilder::new()
            .code_u8(DistributedCode::BranchRoot.as_u8())
            .string(&self.username)
            .build()
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        Ok(Self {
            username: r.string()?,
        })
    }
}

/// The depth of the sender's subtree, reported to its parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DistributedChildDepth {
    pub depth: i32,
}

impl DistributedChildDepth {
    pub fn to_bytes(&self) -> Bytes {
        MessageBuilder::new()
            .code_u8(DistributedCode::ChildDepth.as_u8())
            .i32(self.depth)
            .build()
    }

    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        Ok(Self { depth: r.i32()? })
    }
}

/// A distributed message wrapped in server message 93.
///
/// The server sends it to branch roots; some (older) clients forward it
/// verbatim to their children, so it also arrives on distributed connections.
/// On the wire it always carries a `u32` code `93`, followed by the `u8`
/// distributed code and the embedded message payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmbeddedMessage {
    /// Code of the embedded distributed message.
    pub distributed_code: u8,
    /// Payload of the embedded distributed message (after its code).
    pub payload: Bytes,
}

impl EmbeddedMessage {
    const CODE: u32 = DistributedCode::EmbeddedMessage as u32;

    /// Full frame in its wire form (`u32` code 93).
    pub fn to_bytes(&self) -> Bytes {
        MessageBuilder::new()
            .code_u32(Self::CODE)
            .u8(self.distributed_code)
            .bytes(&self.payload)
            .build()
    }

    /// Decodes from a distributed connection, where the reader has consumed
    /// only the first byte of the `u32` code.
    pub fn decode(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let rest = r.bytes(3)?;
        if rest.iter().any(|b| *b != 0) {
            let received = u32::from_le_bytes([Self::CODE as u8, rest[0], rest[1], rest[2]]);
            return Err(ProtocolError::UnexpectedCode {
                expected: Self::CODE,
                received,
            });
        }
        Self::decode_after_server_code(r)
    }

    /// Decodes from a server connection, where the reader has consumed the full `u32` code.
    pub fn decode_after_server_code(r: &mut MessageReader) -> Result<Self, ProtocolError> {
        let distributed_code = r.u8()?;
        let payload = r.bytes(r.remaining())?;
        Ok(Self {
            distributed_code,
            payload,
        })
    }

    /// Body (code + payload, no length prefix) of the embedded distributed message.
    pub fn distributed_body(&self) -> Bytes {
        let mut out = BytesMut::with_capacity(1 + self.payload.len());
        out.put_u8(self.distributed_code);
        out.put_slice(&self.payload);
        out.freeze()
    }

    /// Length-prefixed frame of the unwrapped distributed message, suitable for
    /// broadcasting to distributed children.
    pub fn distributed_frame(&self) -> Bytes {
        let len = 1 + self.payload.len();
        let mut out = BytesMut::with_capacity(4 + len);
        out.put_u32_le(len as u32);
        out.put_u8(self.distributed_code);
        out.put_slice(&self.payload);
        out.freeze()
    }

    /// Decodes the embedded distributed message.
    pub fn decode_distributed(&self) -> Result<DistributedIncoming, ProtocolError> {
        DistributedIncoming::decode(self.distributed_body())
    }
}

/// Incoming distributed message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DistributedIncoming {
    /// Code 0; a request carries no token (decoded as `0`).
    Ping(DistributedPingResponse),
    SearchRequest(DistributedSearchRequest),
    BranchLevel(DistributedBranchLevel),
    BranchRoot(DistributedBranchRoot),
    ChildDepth(DistributedChildDepth),
    EmbeddedMessage(EmbeddedMessage),
    /// Unrecognised or unhandled code (including code 6); `body` is the full frame body.
    Unknown {
        code: u8,
        body: Bytes,
    },
}

impl DistributedIncoming {
    pub fn decode(body: Bytes) -> Result<Self, ProtocolError> {
        let mut r = MessageReader::new(body.clone());
        let code = r.code_u8()?;
        Ok(match DistributedCode::try_from(code) {
            Ok(DistributedCode::Ping) => Self::Ping(DistributedPingResponse::decode(&mut r)?),
            Ok(DistributedCode::SearchRequest) => {
                Self::SearchRequest(DistributedSearchRequest::decode(&mut r)?)
            }
            Ok(DistributedCode::BranchLevel) => {
                Self::BranchLevel(DistributedBranchLevel::decode(&mut r)?)
            }
            Ok(DistributedCode::BranchRoot) => {
                Self::BranchRoot(DistributedBranchRoot::decode(&mut r)?)
            }
            Ok(DistributedCode::ChildDepth) => {
                Self::ChildDepth(DistributedChildDepth::decode(&mut r)?)
            }
            Ok(DistributedCode::EmbeddedMessage) => {
                Self::EmbeddedMessage(EmbeddedMessage::decode(&mut r)?)
            }
            Ok(DistributedCode::Unknown) | Err(_) => Self::Unknown { code, body },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(frame: Bytes) -> Bytes {
        frame.slice(4..)
    }

    fn decode(frame: Bytes) -> DistributedIncoming {
        DistributedIncoming::decode(body(frame)).unwrap()
    }

    #[test]
    fn search_request_round_trip() {
        let m = DistributedSearchRequest {
            username: "alice".into(),
            token: 0xDEAD_BEEF,
            query: "foo bar -baz".into(),
        };
        let frame = m.to_bytes();
        assert_eq!(frame.len(), 4 + 1 + 4 + 4 + 5 + 4 + 4 + 12);
        assert_eq!(&frame[4..10], &[3, 0, 0, 0, 0, 5]);
        assert_eq!(decode(frame), DistributedIncoming::SearchRequest(m));
    }

    #[test]
    fn search_request_ignores_leading_integer() {
        let frame = MessageBuilder::new()
            .code_u8(3)
            .u32(42)
            .string("bob")
            .u32(7)
            .string("q")
            .build();
        assert_eq!(
            decode(frame),
            DistributedIncoming::SearchRequest(DistributedSearchRequest {
                username: "bob".into(),
                token: 7,
                query: "q".into(),
            })
        );
    }

    #[test]
    fn ping_request_and_response() {
        let req = DistributedPingRequest.to_bytes();
        assert_eq!(&req[..], &[1, 0, 0, 0, 0]);
        assert_eq!(
            decode(req),
            DistributedIncoming::Ping(DistributedPingResponse { token: 0 })
        );

        let resp = DistributedPingResponse { token: 1234 };
        let frame = resp.to_bytes();
        assert_eq!(frame.len(), 4 + 1 + 4);
        assert_eq!(decode(frame), DistributedIncoming::Ping(resp));

        let mut r = MessageReader::new(Bytes::new());
        assert_eq!(
            DistributedPingRequest::decode(&mut r).unwrap(),
            DistributedPingRequest
        );
    }

    #[test]
    fn branch_level_round_trip() {
        let m = DistributedBranchLevel { level: -3 };
        let frame = m.to_bytes();
        assert_eq!(&frame[..], &[5, 0, 0, 0, 4, 0xFD, 0xFF, 0xFF, 0xFF]);
        assert_eq!(decode(frame), DistributedIncoming::BranchLevel(m));
    }

    #[test]
    fn branch_root_round_trip() {
        let m = DistributedBranchRoot {
            username: "root".into(),
        };
        let frame = m.to_bytes();
        assert_eq!(frame.len(), 4 + 1 + 4 + 4);
        assert_eq!(decode(frame), DistributedIncoming::BranchRoot(m));
    }

    #[test]
    fn child_depth_round_trip() {
        let m = DistributedChildDepth { depth: 2 };
        let frame = m.to_bytes();
        assert_eq!(&frame[..], &[5, 0, 0, 0, 7, 2, 0, 0, 0]);
        assert_eq!(decode(frame), DistributedIncoming::ChildDepth(m));
    }

    #[test]
    fn embedded_message_on_distributed_connection() {
        // as built by Soulseek.NET tests: server code 93, byte 3, then the search payload
        let frame = MessageBuilder::new()
            .code_u32(93)
            .u8(3)
            .bytes(&[0; 4])
            .string("carol")
            .u32(99)
            .string("some query")
            .build();
        let DistributedIncoming::EmbeddedMessage(e) = decode(frame.clone()) else {
            panic!("expected embedded message");
        };
        assert_eq!(e.distributed_code, 3);
        assert_eq!(e.to_bytes(), frame);

        let expected = DistributedSearchRequest {
            username: "carol".into(),
            token: 99,
            query: "some query".into(),
        };
        assert_eq!(e.distributed_frame(), expected.to_bytes());
        assert_eq!(e.distributed_body(), body(expected.to_bytes()));
        assert_eq!(
            e.decode_distributed().unwrap(),
            DistributedIncoming::SearchRequest(expected)
        );
    }

    #[test]
    fn embedded_message_from_server() {
        let frame = MessageBuilder::new().code_u32(93).u8(4).i32(1).build();
        let mut r = MessageReader::new(body(frame));
        assert_eq!(r.code_u32().unwrap(), 93);
        let e = EmbeddedMessage::decode_after_server_code(&mut r).unwrap();
        assert_eq!(e.distributed_code, 4);
        assert_eq!(&e.payload[..], &[1, 0, 0, 0]);
        assert_eq!(
            e.decode_distributed().unwrap(),
            DistributedIncoming::BranchLevel(DistributedBranchLevel { level: 1 })
        );
    }

    #[test]
    fn embedded_message_errors() {
        // missing distributed code
        let frame = MessageBuilder::new().code_u32(93).build();
        assert!(DistributedIncoming::decode(body(frame)).is_err());

        // u8 code 93 not followed by the rest of the u32 server code
        let frame = MessageBuilder::new()
            .code_u8(93)
            .u8(1)
            .u8(0)
            .u8(0)
            .u8(3)
            .build();
        assert!(matches!(
            DistributedIncoming::decode(body(frame)),
            Err(ProtocolError::UnexpectedCode { expected: 93, .. })
        ));
    }

    #[test]
    fn unknown_codes() {
        for code in [6u8, 1, 200] {
            let b = Bytes::from(vec![code, 1, 2, 3]);
            assert_eq!(
                DistributedIncoming::decode(b.clone()).unwrap(),
                DistributedIncoming::Unknown { code, body: b }
            );
        }
        assert!(DistributedIncoming::decode(Bytes::new()).is_err());
    }

    #[test]
    fn truncated_messages_fail() {
        assert!(DistributedIncoming::decode(Bytes::from_static(&[4, 1, 0])).is_err());
        assert!(DistributedIncoming::decode(Bytes::from_static(&[3, 0, 0, 0, 0])).is_err());
        assert!(DistributedIncoming::decode(Bytes::from_static(&[5])).is_err());
    }
}
