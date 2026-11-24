//! Message framing: [`MessageBuilder`], [`MessageReader`] and [`ProtocolError`].

use std::cell::RefCell;
use std::io;
use std::net::Ipv4Addr;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use flate2::{Compress, Compression, Decompress, FlushCompress, FlushDecompress, Status};

/// Maximum inflated peer payload size, independent of the compressed frame size.
const MAX_INFLATED_PAYLOAD_LEN: usize = 256 * 1024 * 1024;
/// Upper bound of the up-front inflate buffer; larger payloads grow as they inflate,
/// so a small hostile frame cannot reserve much memory before producing output.
const MAX_INFLATE_PREALLOCATION: usize = 1024 * 1024;

// zlib states are large (the deflate state is a few hundred KiB) and costly to set
// up relative to typical small search responses, so each thread reuses one of each.
thread_local! {
    static DEFLATE: RefCell<Option<Compress>> = const { RefCell::new(None) };
    static INFLATE: RefCell<Option<Decompress>> = const { RefCell::new(None) };
}

/// Zlib-compresses `input` (zlib header). The default level reuses this thread's state.
fn deflate(input: &[u8], level: Compression) -> Vec<u8> {
    fn run(state: &mut Compress, input: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(input.len() / 2 + 64);
        loop {
            // Ample space guarantees progress on every call.
            if out.capacity() - out.len() < 1024 {
                out.reserve(out.capacity().max(1024));
            }
            let consumed = state.total_in() as usize;
            // Compressing into memory cannot fail.
            match state
                .compress_vec(&input[consumed..], &mut out, FlushCompress::Finish)
                .expect("in-memory zlib compression")
            {
                Status::StreamEnd => return out,
                Status::Ok | Status::BufError => {}
            }
        }
    }
    if level != Compression::default() {
        return run(&mut Compress::new(level, true), input);
    }
    DEFLATE.with(|cell| match cell.try_borrow_mut() {
        Ok(mut slot) => {
            let state = slot.get_or_insert_with(|| Compress::new(Compression::default(), true));
            // Also clears anything left by an earlier call that unwound mid-stream.
            state.reset();
            run(state, input)
        }
        Err(_) => run(&mut Compress::new(Compression::default(), true), input),
    })
}

/// Inflates a zlib stream of at most `limit` bytes, reusing this thread's state.
/// Data after the end of the stream is ignored.
fn inflate(input: &[u8], limit: usize) -> Result<Vec<u8>, ProtocolError> {
    fn run(state: &mut Decompress, input: &[u8], limit: usize) -> Result<Vec<u8>, ProtocolError> {
        let corrupt =
            |error| ProtocolError::Decompress(io::Error::new(io::ErrorKind::InvalidInput, error));
        let too_large =
            || ProtocolError::Invalid(format!("inflated payload exceeds {limit} bytes"));
        let hint = input
            .len()
            .saturating_mul(4)
            .clamp(64, MAX_INFLATE_PREALLOCATION)
            .min(limit);
        let mut out = Vec::with_capacity(hint);
        loop {
            if out.len() >= limit {
                if out.len() > limit {
                    return Err(too_large());
                }
                // Probe past the cap without growing the output buffer.
                let consumed = state.total_in() as usize;
                let before = state.total_out();
                let status = state
                    .decompress(&input[consumed..], &mut [0], FlushDecompress::None)
                    .map_err(corrupt)?;
                return match status {
                    _ if state.total_out() != before => Err(too_large()),
                    Status::StreamEnd => Ok(out),
                    Status::Ok | Status::BufError => Err(incomplete()),
                };
            }
            if out.len() == out.capacity() {
                out.reserve_exact(out.capacity().max(4096).min(limit - out.len()));
            }
            let consumed = state.total_in() as usize;
            let before = (state.total_in(), state.total_out());
            match state
                .decompress_vec(&input[consumed..], &mut out, FlushDecompress::None)
                .map_err(corrupt)?
            {
                Status::StreamEnd if out.len() > limit => return Err(too_large()),
                Status::StreamEnd => return Ok(out),
                // Without progress despite free output space, the input ended early.
                Status::Ok | Status::BufError
                    if (state.total_in(), state.total_out()) == before
                        && out.len() < out.capacity() =>
                {
                    return Err(incomplete());
                }
                Status::Ok | Status::BufError => {}
            }
        }
    }
    INFLATE.with(|cell| match cell.try_borrow_mut() {
        Ok(mut slot) => {
            let state = slot.get_or_insert_with(|| Decompress::new(true));
            state.reset(true);
            run(state, input, limit)
        }
        Err(_) => run(&mut Decompress::new(true), input, limit),
    })
}

fn incomplete() -> ProtocolError {
    ProtocolError::Decompress(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "incomplete deflate stream",
    ))
}

/// Errors produced while encoding or decoding protocol messages.
#[derive(Debug, thiserror::Error)]
pub enum ProtocolError {
    #[error("message truncated: needed {needed} bytes, {remaining} remaining")]
    Truncated { needed: usize, remaining: usize },
    #[error("unknown message code {code}")]
    UnknownCode { code: u32 },
    #[error("failed to decompress message: {0}")]
    Decompress(#[source] std::io::Error),
    #[error("unexpected message code: expected {expected}, received {received}")]
    UnexpectedCode { expected: u32, received: u32 },
    #[error("invalid message: {0}")]
    Invalid(String),
}

/// Builds a length-prefixed message frame.
#[derive(Debug, Clone, Default)]
pub struct MessageBuilder {
    code: BytesMut,
    payload: BytesMut,
}

impl MessageBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Writes a 4-byte code (server/peer messages).
    pub fn code_u32(&mut self, code: u32) -> &mut Self {
        self.code.clear();
        self.code.put_u32_le(code);
        self
    }

    /// Writes a 1-byte code (init/distributed messages).
    pub fn code_u8(&mut self, code: u8) -> &mut Self {
        self.code.clear();
        self.code.put_u8(code);
        self
    }

    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.payload.put_u8(v);
        self
    }

    /// Writes a bool as a single byte (0/1).
    pub fn bool(&mut self, v: bool) -> &mut Self {
        self.payload.put_u8(v as u8);
        self
    }

    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.payload.put_u16_le(v);
        self
    }

    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.payload.put_u32_le(v);
        self
    }

    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.payload.put_i32_le(v);
        self
    }

    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.payload.put_u64_le(v);
        self
    }

    /// Writes a length-prefixed UTF-8 string.
    pub fn string(&mut self, v: &str) -> &mut Self {
        self.payload.put_u32_le(v.len() as u32);
        self.payload.put_slice(v.as_bytes());
        self
    }

    /// Writes raw bytes without a length prefix.
    pub fn bytes(&mut self, v: &[u8]) -> &mut Self {
        self.payload.put_slice(v);
        self
    }

    /// Writes an IPv4 address in protocol (reversed) byte order.
    pub fn ip(&mut self, v: Ipv4Addr) -> &mut Self {
        self.payload.put_u32_le(u32::from(v));
        self
    }

    /// Zlib-compresses the payload written so far (everything after the code).
    pub fn compress(&mut self) -> &mut Self {
        self.compress_with(Compression::default())
    }

    /// Like [`MessageBuilder::compress`] at the highest zlib level: several times
    /// slower for a few percent fewer bytes, for frames encoded once and sent often.
    pub fn compress_best(&mut self) -> &mut Self {
        self.compress_with(Compression::best())
    }

    fn compress_with(&mut self, level: Compression) -> &mut Self {
        let compressed = deflate(&self.payload, level);
        self.payload.clear();
        self.payload.extend_from_slice(&compressed);
        self
    }

    /// Returns the complete frame: `u32` length, code, payload.
    pub fn build(&self) -> Bytes {
        let len = self.code.len() + self.payload.len();
        let mut out = BytesMut::with_capacity(4 + len);
        out.put_u32_le(len as u32);
        out.put_slice(&self.code);
        out.put_slice(&self.payload);
        out.freeze()
    }
}

/// Reads values from a single frame body (no length prefix).
#[derive(Debug, Clone)]
pub struct MessageReader {
    buf: Bytes,
}

impl MessageReader {
    pub fn new(body: Bytes) -> Self {
        Self { buf: body }
    }

    fn need(&self, n: usize) -> Result<(), ProtocolError> {
        if self.buf.remaining() < n {
            Err(ProtocolError::Truncated {
                needed: n,
                remaining: self.buf.remaining(),
            })
        } else {
            Ok(())
        }
    }

    pub fn code_u32(&mut self) -> Result<u32, ProtocolError> {
        self.u32()
    }

    pub fn code_u8(&mut self) -> Result<u8, ProtocolError> {
        self.u8()
    }

    pub fn u8(&mut self) -> Result<u8, ProtocolError> {
        self.need(1)?;
        Ok(self.buf.get_u8())
    }

    /// Reads a single byte; any non-zero value is `true`.
    pub fn bool(&mut self) -> Result<bool, ProtocolError> {
        Ok(self.u8()? != 0)
    }

    pub fn u16(&mut self) -> Result<u16, ProtocolError> {
        self.need(2)?;
        Ok(self.buf.get_u16_le())
    }

    pub fn u32(&mut self) -> Result<u32, ProtocolError> {
        self.need(4)?;
        Ok(self.buf.get_u32_le())
    }

    pub fn i32(&mut self) -> Result<i32, ProtocolError> {
        self.need(4)?;
        Ok(self.buf.get_i32_le())
    }

    pub fn u64(&mut self) -> Result<u64, ProtocolError> {
        self.need(8)?;
        Ok(self.buf.get_u64_le())
    }

    /// Reads a length-prefixed string; UTF-8 with ISO-8859-1 fallback (each byte maps
    /// to the code point of the same value), matching Soulseek.NET.
    pub fn string(&mut self) -> Result<String, ProtocolError> {
        let len = self.u32()? as usize;
        let raw = self.bytes(len)?;
        Ok(match std::str::from_utf8(&raw) {
            Ok(s) => s.to_owned(),
            Err(_) => raw.iter().map(|&b| char::from(b)).collect(),
        })
    }

    /// Reads `n` raw bytes.
    pub fn bytes(&mut self, n: usize) -> Result<Bytes, ProtocolError> {
        self.need(n)?;
        Ok(self.buf.split_to(n))
    }

    /// Reads an IPv4 address stored in protocol (reversed) byte order.
    pub fn ip(&mut self) -> Result<Ipv4Addr, ProtocolError> {
        Ok(Ipv4Addr::from(self.u32()?))
    }

    pub fn remaining(&self) -> usize {
        self.buf.remaining()
    }

    pub fn has_remaining(&self) -> bool {
        self.buf.has_remaining()
    }

    /// Zlib-inflates the unread remainder in place, rejecting payloads over 256 MiB.
    pub fn decompress(&mut self) -> Result<(), ProtocolError> {
        self.decompress_with_limit(MAX_INFLATED_PAYLOAD_LEN)
    }

    fn decompress_with_limit(&mut self, limit: usize) -> Result<(), ProtocolError> {
        self.buf = Bytes::from(inflate(&self.buf, limit)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(frame: Bytes) -> Bytes {
        let mut f = frame;
        let len = f.get_u32_le() as usize;
        assert_eq!(len, f.len());
        f
    }

    #[test]
    fn build_frames_length_and_code() {
        let b = MessageBuilder::new().code_u32(5).u32(7).build();
        assert_eq!(&b[..], &[8, 0, 0, 0, 5, 0, 0, 0, 7, 0, 0, 0]);
        let b = MessageBuilder::new().code_u8(1).build();
        assert_eq!(&b[..], &[1, 0, 0, 0, 1]);
    }

    #[test]
    fn primitives_round_trip() {
        let frame = MessageBuilder::new()
            .code_u32(42)
            .u8(0xAB)
            .bool(true)
            .bool(false)
            .u16(0xBEEF)
            .u32(0xDEADBEEF)
            .i32(-5)
            .u64(0x0102_0304_0506_0708)
            .string("héllo")
            .bytes(&[9, 8, 7])
            .ip(Ipv4Addr::new(192, 168, 1, 2))
            .build();
        let mut r = MessageReader::new(body(frame));
        assert_eq!(r.code_u32().unwrap(), 42);
        assert_eq!(r.u8().unwrap(), 0xAB);
        assert!(r.bool().unwrap());
        assert!(!r.bool().unwrap());
        assert_eq!(r.u16().unwrap(), 0xBEEF);
        assert_eq!(r.u32().unwrap(), 0xDEADBEEF);
        assert_eq!(r.i32().unwrap(), -5);
        assert_eq!(r.u64().unwrap(), 0x0102_0304_0506_0708);
        assert_eq!(r.string().unwrap(), "héllo");
        assert_eq!(&r.bytes(3).unwrap()[..], &[9, 8, 7]);
        assert_eq!(r.ip().unwrap(), Ipv4Addr::new(192, 168, 1, 2));
        assert!(!r.has_remaining());
        assert_eq!(r.remaining(), 0);
    }

    #[test]
    fn little_endian_layout() {
        let f = MessageBuilder::new().code_u8(0).u16(0x0102).u64(1).build();
        assert_eq!(&f[5..], &[2, 1, 1, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn ip_reversed_on_wire() {
        let f = MessageBuilder::new()
            .code_u8(0)
            .ip(Ipv4Addr::new(1, 2, 3, 4))
            .build();
        assert_eq!(&f[5..], &[4, 3, 2, 1]);
        let mut r = MessageReader::new(Bytes::from_static(&[4, 3, 2, 1]));
        assert_eq!(r.ip().unwrap(), Ipv4Addr::new(1, 2, 3, 4));
    }

    #[test]
    fn string_utf8_and_fallback() {
        let mut r = MessageReader::new(Bytes::from_static(&[3, 0, 0, 0, b'a', 0xE9, b'b']));
        assert_eq!(r.string().unwrap(), "aéb");
        let mut r = MessageReader::new(Bytes::from_static(&[2, 0, 0, 0, 0xC3, 0xA9]));
        assert_eq!(r.string().unwrap(), "é");
        let mut r = MessageReader::new(Bytes::from_static(&[0, 0, 0, 0]));
        assert_eq!(r.string().unwrap(), "");
        // ISO-8859-1, not Windows-1252: 0x80-0x9F decode to C1 controls.
        let mut r = MessageReader::new(Bytes::from_static(&[3, 0, 0, 0, 0x80, 0x9F, 0xFF]));
        assert_eq!(r.string().unwrap(), "\u{80}\u{9F}\u{FF}");
    }

    #[test]
    fn compression_round_trip() {
        let frame = MessageBuilder::new()
            .code_u32(9)
            .string("user")
            .u32(1234)
            .bytes(&[0u8; 256])
            .compress()
            .build();
        let mut r = MessageReader::new(body(frame));
        assert_eq!(r.code_u32().unwrap(), 9);
        r.decompress().unwrap();
        assert_eq!(r.string().unwrap(), "user");
        assert_eq!(r.u32().unwrap(), 1234);
        assert_eq!(r.remaining(), 256);
    }

    #[test]
    fn decompress_enforces_inflated_size_limit() {
        let payload = b"payload";
        let frame = MessageBuilder::new()
            .code_u8(0)
            .bytes(payload)
            .compress()
            .build();
        let compressed = body(frame).slice(1..);

        let mut r = MessageReader::new(compressed.clone());
        r.decompress_with_limit(payload.len()).unwrap();
        assert_eq!(&r.buf[..], payload);

        let mut r = MessageReader::new(compressed.clone());
        assert!(matches!(
            r.decompress_with_limit(payload.len() - 1),
            Err(ProtocolError::Invalid(_))
        ));
        assert_eq!(r.buf, compressed);
    }

    #[test]
    fn decompress_garbage_fails() {
        let mut r = MessageReader::new(Bytes::from_static(&[1, 2, 3, 4, 5]));
        assert!(matches!(r.decompress(), Err(ProtocolError::Decompress(_))));
    }

    /// Deterministic, poorly compressible bytes, so inflating grows the buffer repeatedly.
    fn noise(len: usize) -> Vec<u8> {
        let mut state = 0x1234_5678_u32;
        (0..len)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                state as u8
            })
            .collect()
    }

    fn reference_deflate(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    #[test]
    fn reused_zlib_state_matches_a_fresh_encoder_and_decoder() {
        use std::io::Read;
        let large = noise(3 * MAX_INFLATE_PREALLOCATION + 17);
        for data in [&b""[..], b"x", &[7; 70_000], &large] {
            // Repeat on this thread's reused state.
            for _ in 0..2 {
                let compressed = deflate(data, Compression::default());
                assert_eq!(compressed, reference_deflate(data));
                let mut inflated = Vec::new();
                flate2::read::ZlibDecoder::new(&compressed[..])
                    .read_to_end(&mut inflated)
                    .unwrap();
                assert_eq!(inflated, data);
                assert_eq!(
                    inflate(&compressed, MAX_INFLATED_PAYLOAD_LEN).unwrap(),
                    data
                );
            }
        }
    }

    #[test]
    fn decompress_rejects_truncated_streams_and_ignores_trailing_data() {
        let data = noise(10_000);
        let compressed = reference_deflate(&data);
        for cut in [0, 1, compressed.len() / 2, compressed.len() - 1] {
            assert!(
                matches!(
                    inflate(&compressed[..cut], MAX_INFLATED_PAYLOAD_LEN),
                    Err(ProtocolError::Decompress(_))
                ),
                "cut at {cut}"
            );
        }
        let mut trailing = compressed.clone();
        trailing.extend_from_slice(b"trailing");
        assert_eq!(inflate(&trailing, MAX_INFLATED_PAYLOAD_LEN).unwrap(), data);
        // A failure leaves the reused state usable.
        assert_eq!(inflate(&compressed, data.len()).unwrap(), data);
    }

    #[test]
    fn inflate_limit_applies_beyond_the_preallocation() {
        let data = noise(2 * MAX_INFLATE_PREALLOCATION + 5);
        let compressed = reference_deflate(&data);
        assert_eq!(inflate(&compressed, data.len()).unwrap(), data);
        assert!(matches!(
            inflate(&compressed, data.len() - 1),
            Err(ProtocolError::Invalid(_))
        ));
        assert!(matches!(
            inflate(&compressed, MAX_INFLATE_PREALLOCATION),
            Err(ProtocolError::Invalid(_))
        ));
    }

    #[test]
    fn truncated_errors() {
        let mut r = MessageReader::new(Bytes::from_static(&[1, 2, 3]));
        assert!(matches!(
            r.u32(),
            Err(ProtocolError::Truncated {
                needed: 4,
                remaining: 3
            })
        ));
        assert!(matches!(r.u64(), Err(ProtocolError::Truncated { .. })));
        assert_eq!(r.u16().unwrap(), 0x0201);
        assert!(matches!(r.u16(), Err(ProtocolError::Truncated { .. })));
        assert_eq!(r.u8().unwrap(), 3);
        assert!(matches!(r.u8(), Err(ProtocolError::Truncated { .. })));
        assert!(matches!(r.bool(), Err(ProtocolError::Truncated { .. })));
        assert!(matches!(r.i32(), Err(ProtocolError::Truncated { .. })));
        assert!(matches!(r.bytes(1), Err(ProtocolError::Truncated { .. })));
        assert!(matches!(r.ip(), Err(ProtocolError::Truncated { .. })));
        let mut r = MessageReader::new(Bytes::from_static(&[10, 0, 0, 0, b'a']));
        assert!(matches!(r.string(), Err(ProtocolError::Truncated { .. })));
    }
}
