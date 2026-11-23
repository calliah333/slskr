//! ASP.NET Core SignalR JSON hub protocol codec.
//!
//! Every message is a JSON object terminated by the ASCII record separator (`0x1e`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Record separator terminating every protocol message.
pub const RECORD_SEPARATOR: char = '\u{1e}';

/// Highest JSON protocol version accepted in the handshake.
pub const MAX_PROTOCOL_VERSION: u32 = 2;

/// Upper bound for a single record, whether complete or still buffered, and for one
/// WebSocket message/frame. Upstream slskd keeps ASP.NET Core SignalR's default
/// `MaximumReceiveMessageSize` of 32 KiB; legitimate client-to-server messages are far
/// smaller. 1 MiB leaves ample headroom.
pub const MAX_BUFFERED: usize = 1024 * 1024;

/// Message type discriminators.
pub mod message_type {
    pub const INVOCATION: u8 = 1;
    pub const STREAM_ITEM: u8 = 2;
    pub const COMPLETION: u8 = 3;
    pub const STREAM_INVOCATION: u8 = 4;
    pub const CANCEL_INVOCATION: u8 = 5;
    pub const PING: u8 = 6;
    pub const CLOSE: u8 = 7;
}

/// Client handshake request: `{"protocol":"json","version":1}`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HandshakeRequest {
    pub protocol: String,
    pub version: u32,
}

/// Parses a handshake record (without separator).
pub fn parse_handshake(record: &str) -> Result<HandshakeRequest, String> {
    serde_json::from_str(record).map_err(|e| format!("Unable to parse handshake request: {e}"))
}

/// Checks that the requested protocol is one this server speaks.
pub fn validate_handshake(request: &HandshakeRequest) -> Result<(), String> {
    if !request.protocol.eq_ignore_ascii_case("json") {
        return Err(format!(
            "The protocol '{}' is not supported.",
            request.protocol
        ));
    }

    if request.version == 0 || request.version > MAX_PROTOCOL_VERSION {
        return Err(format!(
            "The server does not support version {} of the '{}' protocol.",
            request.version, request.protocol
        ));
    }

    Ok(())
}

/// A decoded client-to-server (or server-to-client) hub message.
#[derive(Debug, Clone, PartialEq)]
pub enum HubMessage {
    Invocation {
        invocation_id: Option<String>,
        target: String,
        arguments: Vec<Value>,
    },
    StreamItem {
        invocation_id: String,
        item: Value,
    },
    Completion {
        invocation_id: String,
        result: Option<Value>,
        error: Option<String>,
    },
    StreamInvocation {
        invocation_id: String,
        target: String,
        arguments: Vec<Value>,
    },
    CancelInvocation {
        invocation_id: String,
    },
    Ping,
    Close {
        error: Option<String>,
        allow_reconnect: bool,
    },
    /// Any other type (e.g. stateful reconnect `Ack`/`Sequence`); ignored.
    Other(u8),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawMessage {
    #[serde(rename = "type")]
    kind: u8,
    #[serde(default)]
    invocation_id: Option<String>,
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    arguments: Option<Vec<Value>>,
    #[serde(default)]
    item: Option<Value>,
    #[serde(default)]
    result: Option<Value>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    allow_reconnect: Option<bool>,
}

/// Parses one hub message record (without separator).
pub fn parse_message(record: &str) -> Result<HubMessage, String> {
    let raw: RawMessage =
        serde_json::from_str(record).map_err(|e| format!("Error reading JSON: {e}"))?;

    let required = |field: Option<String>, name: &str| {
        field.ok_or_else(|| format!("Missing required property '{name}'."))
    };

    let message = match raw.kind {
        message_type::INVOCATION => HubMessage::Invocation {
            invocation_id: raw.invocation_id,
            target: required(raw.target, "target")?,
            arguments: raw.arguments.unwrap_or_default(),
        },
        message_type::STREAM_ITEM => HubMessage::StreamItem {
            invocation_id: required(raw.invocation_id, "invocationId")?,
            item: raw.item.unwrap_or(Value::Null),
        },
        message_type::COMPLETION => HubMessage::Completion {
            invocation_id: required(raw.invocation_id, "invocationId")?,
            result: raw.result,
            error: raw.error,
        },
        message_type::STREAM_INVOCATION => HubMessage::StreamInvocation {
            invocation_id: required(raw.invocation_id, "invocationId")?,
            target: required(raw.target, "target")?,
            arguments: raw.arguments.unwrap_or_default(),
        },
        message_type::CANCEL_INVOCATION => HubMessage::CancelInvocation {
            invocation_id: required(raw.invocation_id, "invocationId")?,
        },
        message_type::PING => HubMessage::Ping,
        message_type::CLOSE => HubMessage::Close {
            error: raw.error,
            allow_reconnect: raw.allow_reconnect.unwrap_or(false),
        },
        other => HubMessage::Other(other),
    };

    Ok(message)
}

fn terminate(mut json: String) -> String {
    json.push(RECORD_SEPARATOR);
    json
}

fn to_record<T: Serialize>(value: &T) -> String {
    // serialising these borrowed structs of JSON values cannot fail
    terminate(serde_json::to_string(value).unwrap_or_default())
}

/// Handshake response: `{}` on success, `{"error":..}` on failure.
pub fn encode_handshake_response(error: Option<&str>) -> String {
    #[derive(Serialize)]
    struct Response<'a> {
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'a str>,
    }

    to_record(&Response { error })
}

/// Non-blocking server-to-client invocation.
pub fn encode_invocation(target: &str, arguments: &[Value]) -> String {
    #[derive(Serialize)]
    struct Invocation<'a> {
        #[serde(rename = "type")]
        kind: u8,
        target: &'a str,
        arguments: &'a [Value],
    }

    to_record(&Invocation {
        kind: message_type::INVOCATION,
        target,
        arguments,
    })
}

/// Successful completion of a client invocation.
pub fn encode_completion_result(invocation_id: &str, result: &Value) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Completion<'a> {
        #[serde(rename = "type")]
        kind: u8,
        invocation_id: &'a str,
        result: &'a Value,
    }

    to_record(&Completion {
        kind: message_type::COMPLETION,
        invocation_id,
        result,
    })
}

/// Failed completion of a client invocation.
pub fn encode_completion_error(invocation_id: &str, error: &str) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Completion<'a> {
        #[serde(rename = "type")]
        kind: u8,
        invocation_id: &'a str,
        error: &'a str,
    }

    to_record(&Completion {
        kind: message_type::COMPLETION,
        invocation_id,
        error,
    })
}

/// Keep-alive ping: `{"type":6}`.
pub fn encode_ping() -> String {
    terminate(format!("{{\"type\":{}}}", message_type::PING))
}

/// Close message; `allowReconnect` is only emitted when true, as ASP.NET does.
pub fn encode_close(error: Option<&str>, allow_reconnect: bool) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Close<'a> {
        #[serde(rename = "type")]
        kind: u8,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<&'a str>,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        allow_reconnect: bool,
    }

    to_record(&Close {
        kind: message_type::CLOSE,
        error,
        allow_reconnect,
    })
}

/// Accumulates transport data and yields complete records.
#[derive(Debug, Default)]
pub struct RecordBuffer {
    buf: String,
}

impl RecordBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends data and returns every complete, non-empty record (separators stripped).
    pub fn push(&mut self, data: &str) -> Result<Vec<String>, String> {
        let too_large = || format!("Message exceeds the maximum size of {MAX_BUFFERED} bytes.");
        self.buf.push_str(data);

        let Some(last) = self.buf.rfind(RECORD_SEPARATOR) else {
            if self.buf.len() > MAX_BUFFERED {
                return Err(too_large());
            }
            return Ok(Vec::new());
        };

        // the separator is a single byte, so `last + 1` is a char boundary
        let rest = self.buf.split_off(last + 1);
        let complete = std::mem::replace(&mut self.buf, rest);
        if self.buf.len() > MAX_BUFFERED {
            return Err(too_large());
        }

        complete[..last]
            .split(RECORD_SEPARATOR)
            .filter(|record| !record.trim().is_empty())
            .map(|record| {
                if record.len() > MAX_BUFFERED {
                    Err(too_large())
                } else {
                    Ok(record.to_owned())
                }
            })
            .collect()
    }

    /// True when no partial record is pending.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn handshake_parses_and_validates() {
        let request = parse_handshake(r#"{"protocol":"json","version":1}"#).unwrap();
        assert_eq!(
            request,
            HandshakeRequest {
                protocol: "json".into(),
                version: 1
            }
        );
        assert!(validate_handshake(&request).is_ok());

        let v2 = parse_handshake(r#"{"protocol":"json","version":2,"extra":true}"#).unwrap();
        assert!(validate_handshake(&v2).is_ok());
    }

    #[test]
    fn handshake_rejects_unsupported() {
        let mp = parse_handshake(r#"{"protocol":"messagepack","version":1}"#).unwrap();
        assert_eq!(
            validate_handshake(&mp).unwrap_err(),
            "The protocol 'messagepack' is not supported."
        );

        let v9 = parse_handshake(r#"{"protocol":"json","version":9}"#).unwrap();
        assert!(validate_handshake(&v9).is_err());

        assert!(parse_handshake(r#"{"version":1}"#).is_err());
        assert!(parse_handshake("not json").is_err());
    }

    #[test]
    fn handshake_response_framing() {
        assert_eq!(encode_handshake_response(None), "{}\u{1e}");
        assert_eq!(
            encode_handshake_response(Some("nope")),
            "{\"error\":\"nope\"}\u{1e}"
        );
    }

    #[test]
    fn encodes_invocation() {
        let frame = encode_invocation("state", &[json!({"a":1}), json!("x")]);
        assert_eq!(
            frame,
            "{\"type\":1,\"target\":\"state\",\"arguments\":[{\"a\":1},\"x\"]}\u{1e}"
        );
    }

    #[test]
    fn encodes_completion_ping_close() {
        assert_eq!(
            encode_completion_result("7", &json!(42)),
            "{\"type\":3,\"invocationId\":\"7\",\"result\":42}\u{1e}"
        );
        assert_eq!(
            encode_completion_error("7", "boom"),
            "{\"type\":3,\"invocationId\":\"7\",\"error\":\"boom\"}\u{1e}"
        );
        assert_eq!(encode_ping(), "{\"type\":6}\u{1e}");
        assert_eq!(encode_close(None, false), "{\"type\":7}\u{1e}");
        assert_eq!(
            encode_close(Some("bye"), true),
            "{\"type\":7,\"error\":\"bye\",\"allowReconnect\":true}\u{1e}"
        );
    }

    #[test]
    fn parses_invocations() {
        let blocking =
            parse_message(r#"{"type":1,"invocationId":"0","target":"Send","arguments":[1,"two"]}"#)
                .unwrap();
        assert_eq!(
            blocking,
            HubMessage::Invocation {
                invocation_id: Some("0".into()),
                target: "Send".into(),
                arguments: vec![json!(1), json!("two")],
            }
        );

        let fire = parse_message(r#"{"type":1,"target":"Notify","arguments":[]}"#).unwrap();
        assert_eq!(
            fire,
            HubMessage::Invocation {
                invocation_id: None,
                target: "Notify".into(),
                arguments: vec![]
            }
        );

        assert!(parse_message(r#"{"type":1,"arguments":[]}"#).is_err());
    }

    #[test]
    fn parses_control_messages() {
        assert_eq!(parse_message(r#"{"type":6}"#).unwrap(), HubMessage::Ping);
        assert_eq!(
            parse_message(r#"{"type":7,"error":"x","allowReconnect":true}"#).unwrap(),
            HubMessage::Close {
                error: Some("x".into()),
                allow_reconnect: true
            }
        );
        assert_eq!(
            parse_message(r#"{"type":5,"invocationId":"3"}"#).unwrap(),
            HubMessage::CancelInvocation {
                invocation_id: "3".into()
            }
        );
        assert_eq!(
            parse_message(r#"{"type":4,"invocationId":"1","target":"S","arguments":[]}"#).unwrap(),
            HubMessage::StreamInvocation {
                invocation_id: "1".into(),
                target: "S".into(),
                arguments: vec![]
            }
        );
        assert_eq!(
            parse_message(r#"{"type":3,"invocationId":"1","result":null}"#).unwrap(),
            HubMessage::Completion {
                invocation_id: "1".into(),
                result: None,
                error: None
            }
        );
        assert_eq!(
            parse_message(r#"{"type":8,"sequenceId":1}"#).unwrap(),
            HubMessage::Other(8)
        );
        assert!(parse_message(r#"{"target":"x"}"#).is_err());
    }

    #[test]
    fn record_buffer_splits_and_keeps_partials() {
        let mut buffer = RecordBuffer::new();
        assert!(buffer.push("{\"protocol\":\"json\",").unwrap().is_empty());
        assert!(!buffer.is_empty());

        let records = buffer
            .push("\"version\":1}\u{1e}{\"type\":6}\u{1e}{\"type\"")
            .unwrap();
        assert_eq!(
            records,
            vec!["{\"protocol\":\"json\",\"version\":1}", "{\"type\":6}"]
        );

        let records = buffer.push(":7}\u{1e}\u{1e}").unwrap();
        assert_eq!(records, vec!["{\"type\":7}"]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn record_buffer_caps_records_with_and_without_separators() {
        let big = "x".repeat(MAX_BUFFERED + 1);
        assert!(RecordBuffer::new().push(&big).is_err());
        assert!(RecordBuffer::new().push(&format!("{big}\u{1e}")).is_err());
        assert!(
            RecordBuffer::new()
                .push(&format!("{{}}\u{1e}{big}"))
                .is_err()
        );
        let fits = "x".repeat(MAX_BUFFERED);
        assert_eq!(
            RecordBuffer::new().push(&format!("{fits}\u{1e}")).unwrap(),
            vec![fits]
        );
    }

    #[test]
    fn record_buffer_handles_multibyte() {
        let mut buffer = RecordBuffer::new();
        let records = buffer.push("{\"a\":\"ü😀\"}\u{1e}{\"b\":\"é").unwrap();
        assert_eq!(records, vec!["{\"a\":\"ü😀\"}"]);
        assert_eq!(buffer.push("\"}\u{1e}").unwrap(), vec!["{\"b\":\"é\"}"]);
    }
}
