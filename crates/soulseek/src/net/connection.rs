use std::future::Future;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::{Buf, Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::time::{Instant, timeout, timeout_at};

use crate::error::{Error, Result};
use crate::options::{ConnectionOptions, ProxyOptions};

// Accommodate large compressed share listings without honoring arbitrary u32 sizes.
pub(super) const MAX_FRAME_BODY_LEN: usize = 64 * 1024 * 1024;
const FRAME_HEADER_LEN: usize = size_of::<u32>();
const FRAME_CODE_LEN: usize = size_of::<u32>();

/// Progress of a frame body being read (code and payload, without the length prefix).
pub(crate) struct FrameProgress {
    pub code: u32,
    pub received: usize,
    pub total: usize,
    /// Set on the first report for a frame, as soon as its code is known.
    pub first: bool,
}

/// Invoked by a message connection's reader while frames arrive.
pub(crate) type FrameObserver = Arc<dyn Fn(&FrameProgress) + Send + Sync>;

pub(crate) struct Connection {
    stream: TcpStream,
    remote: SocketAddr,
    reader: ReadBuffer,
    inactivity: Option<Inactivity>,
    write_queue_size: usize,
}

impl Connection {
    pub async fn connect(addr: SocketAddr, opts: &ConnectionOptions) -> Result<Self> {
        let stream = timeout(opts.connect_timeout, async {
            match &opts.proxy {
                Some(proxy) => connect_socks5(addr, proxy).await,
                None => TcpStream::connect(addr).await,
            }
        })
        .await
        .map_err(|_| Error::Timeout(format!("Connecting to {addr} timed out")))?
        .map_err(|err| Error::Connection(format!("Connecting to {addr}: {err}")))?;
        Ok(Self::from_accepted(stream, addr, opts))
    }

    pub fn from_accepted(stream: TcpStream, remote: SocketAddr, opts: &ConnectionOptions) -> Self {
        Self {
            stream,
            remote,
            reader: ReadBuffer::new(opts.read_buffer_size),
            inactivity: opts.inactivity_timeout.map(Inactivity::new),
            write_queue_size: opts.write_queue_size,
        }
    }

    pub fn remote(&self) -> SocketAddr {
        self.remote
    }

    /// Reads a body without consuming even the length prefix until the whole frame is ready.
    pub async fn read_frame(&mut self) -> Result<Bytes> {
        self.reader
            .read_frame(&mut self.stream, self.inactivity.as_ref())
            .await
    }

    pub async fn write_all(&mut self, bytes: &[u8]) -> Result<()> {
        write_all(&mut self.stream, self.inactivity.as_ref(), bytes).await
    }

    /// On cancellation, both the bytes consumed so far and the caller's buffer are unchanged
    /// from the caller's perspective: consumed bytes remain in the connection's read buffer.
    pub async fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.reader
            .read_exact(&mut self.stream, self.inactivity.as_ref(), buf)
            .await
    }

    pub fn into_parts(self) -> (TcpStream, BytesMut) {
        (self.stream, self.reader.bytes)
    }

    pub(super) fn write_queue_size(&self) -> usize {
        self.write_queue_size
    }

    pub(super) fn into_framed_parts(
        self,
    ) -> (
        OwnedReadHalf,
        OwnedWriteHalf,
        ReadBuffer,
        Option<Arc<Inactivity>>,
    ) {
        let (read, write) = self.stream.into_split();
        (read, write, self.reader, self.inactivity.map(Arc::new))
    }
}

pub(super) struct ReadBuffer {
    bytes: BytesMut,
    chunk_size: usize,
}

impl ReadBuffer {
    fn new(chunk_size: usize) -> Self {
        Self {
            bytes: BytesMut::new(),
            chunk_size: chunk_size.clamp(1, MAX_FRAME_BODY_LEN),
        }
    }

    pub(super) async fn read_frame(
        &mut self,
        input: &mut (impl AsyncRead + Unpin),
        inactivity: Option<&Inactivity>,
    ) -> Result<Bytes> {
        self.read_frame_observed(input, inactivity, None).await
    }

    /// Like [`Self::read_frame`], additionally reporting the progress of frames with a
    /// code (body of at least 4 bytes) to `observer` once the code is known and after
    /// each further read until the body is complete.
    pub(super) async fn read_frame_observed(
        &mut self,
        input: &mut (impl AsyncRead + Unpin),
        inactivity: Option<&Inactivity>,
        observer: Option<&FrameObserver>,
    ) -> Result<Bytes> {
        self.fill_until(input, inactivity, FRAME_HEADER_LEN).await?;
        let len = frame_body_len((&self.bytes[..FRAME_HEADER_LEN]).try_into().unwrap())?;
        let end = FRAME_HEADER_LEN + len;
        match observer {
            Some(observer) if len >= FRAME_CODE_LEN => {
                self.fill_until(input, inactivity, FRAME_HEADER_LEN + FRAME_CODE_LEN)
                    .await?;
                let code = u32::from_le_bytes(
                    self.bytes[FRAME_HEADER_LEN..FRAME_HEADER_LEN + FRAME_CODE_LEN]
                        .try_into()
                        .unwrap(),
                );
                let mut first = true;
                loop {
                    observer(&FrameProgress {
                        code,
                        received: (self.bytes.len() - FRAME_HEADER_LEN).min(len),
                        total: len,
                        first,
                    });
                    if self.bytes.len() >= end {
                        break;
                    }
                    first = false;
                    // One read at a time, so progress is reported per chunk.
                    self.fill_until(input, inactivity, self.bytes.len() + 1)
                        .await?;
                }
            }
            _ => self.fill_until(input, inactivity, end).await?,
        }
        self.bytes.advance(FRAME_HEADER_LEN);
        Ok(self.bytes.split_to(len).freeze())
    }

    async fn read_exact(
        &mut self,
        input: &mut (impl AsyncRead + Unpin),
        inactivity: Option<&Inactivity>,
        buf: &mut [u8],
    ) -> Result<()> {
        self.fill_until(input, inactivity, buf.len()).await?;
        self.bytes.copy_to_slice(buf);
        Ok(())
    }

    async fn fill_until(
        &mut self,
        input: &mut (impl AsyncRead + Unpin),
        inactivity: Option<&Inactivity>,
        len: usize,
    ) -> Result<()> {
        while self.bytes.len() < len {
            if self.bytes.len() == self.bytes.capacity() {
                // Grow as data arrives, not to the size advertised by the remote.
                self.bytes
                    .reserve(self.chunk_size.min(len.max(FRAME_HEADER_LEN)));
            }
            let mut input = (&mut *input).take(self.chunk_size as u64);
            let read = until_idle(inactivity, input.read_buf(&mut self.bytes))
                .await?
                .map_err(|err| Error::Connection(format!("Connection read failed: {err}")))?;
            if read == 0 {
                return Err(Error::Connection("Remote connection closed".into()));
            }
            if let Some(inactivity) = inactivity {
                inactivity.progress();
            }
        }
        Ok(())
    }
}

pub(super) fn frame_body_len(header: &[u8; FRAME_HEADER_LEN]) -> Result<usize> {
    let len = u32::from_le_bytes(*header) as usize;
    if !(1..=MAX_FRAME_BODY_LEN).contains(&len) {
        return Err(Error::Connection(format!("Invalid frame length: {len}")));
    }
    Ok(len)
}

pub(super) async fn write_all(
    output: &mut (impl AsyncWrite + Unpin),
    inactivity: Option<&Inactivity>,
    mut bytes: &[u8],
) -> Result<()> {
    while !bytes.is_empty() {
        let written = until_idle(inactivity, output.write(bytes))
            .await?
            .map_err(|err| Error::Connection(format!("Connection write failed: {err}")))?;
        if written == 0 {
            return Err(Error::Connection(
                "Connection write made no progress".into(),
            ));
        }
        if let Some(inactivity) = inactivity {
            inactivity.progress();
        }
        bytes = &bytes[written..];
    }
    Ok(())
}

pub(super) struct Inactivity {
    timeout: Duration,
    last_activity: Mutex<Instant>,
}

impl Inactivity {
    fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            last_activity: Mutex::new(Instant::now()),
        }
    }

    fn progress(&self) {
        let mut last = self.last_activity.lock().unwrap();
        *last = Instant::now();
    }

    fn deadline(&self) -> Option<Instant> {
        self.last_activity.lock().unwrap().checked_add(self.timeout)
    }
}

/// Continue the same operation if the other half made progress before the old deadline.
pub(super) async fn until_idle<F: Future>(
    inactivity: Option<&Inactivity>,
    future: F,
) -> Result<F::Output> {
    let Some(inactivity) = inactivity else {
        return Ok(future.await);
    };
    let Some(mut deadline) = inactivity.deadline() else {
        return Ok(future.await);
    };
    tokio::pin!(future);
    loop {
        if let Ok(result) = timeout_at(deadline, future.as_mut()).await {
            return Ok(result);
        }
        match inactivity.deadline() {
            Some(next) if next > Instant::now() => deadline = next,
            Some(_) => return Err(Error::Timeout("Connection inactivity timed out".into())),
            None => return Ok(future.await),
        }
    }
}

async fn connect_socks5(addr: SocketAddr, proxy: &ProxyOptions) -> io::Result<TcpStream> {
    let credentials = match (&proxy.username, &proxy.password) {
        (None, None) => None,
        (Some(user), Some(pass))
            if (1..=255).contains(&user.len()) && (1..=255).contains(&pass.len()) =>
        {
            Some((user.as_bytes(), pass.as_bytes()))
        }
        _ => return Err(io::Error::other("Invalid SOCKS5 credentials")),
    };
    let mut stream = TcpStream::connect((proxy.address.as_str(), proxy.port)).await?;
    let method = if credentials.is_some() { 2 } else { 0 };
    stream.write_all(&[5, 1, method]).await?;
    let mut selection = [0; 2];
    stream.read_exact(&mut selection).await?;
    if selection != [5, method] {
        return Err(io::Error::other(
            "SOCKS5 proxy rejected the requested authentication method",
        ));
    }

    if let Some((user, pass)) = credentials {
        let mut auth = [0; 3 + 255 * 2];
        let password_len_at = 2 + user.len();
        let end = password_len_at + 1 + pass.len();
        auth[..2].copy_from_slice(&[1, user.len() as u8]);
        auth[2..password_len_at].copy_from_slice(user);
        auth[password_len_at] = pass.len() as u8;
        auth[password_len_at + 1..end].copy_from_slice(pass);
        stream.write_all(&auth[..end]).await?;
        stream.read_exact(&mut selection).await?;
        if selection != [1, 0] {
            return Err(io::Error::other("SOCKS5 proxy authentication failed"));
        }
    }

    let mut request = [0; 22];
    request[..3].copy_from_slice(&[5, 1, 0]);
    let port_at = match addr.ip() {
        IpAddr::V4(ip) => {
            request[3] = 1;
            request[4..8].copy_from_slice(&ip.octets());
            8
        }
        IpAddr::V6(ip) => {
            request[3] = 4;
            request[4..20].copy_from_slice(&ip.octets());
            20
        }
    };
    request[port_at..port_at + 2].copy_from_slice(&addr.port().to_be_bytes());
    stream.write_all(&request[..port_at + 2]).await?;

    let mut reply = [0; 4];
    stream.read_exact(&mut reply).await?;
    if reply[0] != 5 || reply[2] != 0 {
        return Err(io::Error::other("Invalid SOCKS5 CONNECT response"));
    }
    if reply[1] != 0 {
        return Err(io::Error::other(format!(
            "SOCKS5 CONNECT failed with status {:#04x}",
            reply[1]
        )));
    }
    let addr_len = match reply[3] {
        1 => 4,
        4 => 16,
        3 => usize::from(stream.read_u8().await?),
        _ => {
            return Err(io::Error::other(
                "Invalid address type in SOCKS5 CONNECT response",
            ));
        }
    };
    let mut bound = [0; 255 + 2];
    stream.read_exact(&mut bound[..addr_len + 2]).await?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use std::task::Poll;

    use futures::poll;
    use tokio::io::duplex;
    use tokio::time::advance;

    use super::*;
    use crate::net::test_support::FakeServer;

    #[tokio::test]
    async fn cancelled_frame_reads_retain_partial_header_body_and_read_ahead() {
        let (mut input, mut peer) = duplex(32);
        let mut reader = ReadBuffer::new(16);
        peer.write_all(&[5, 0]).await.unwrap();
        {
            let read = reader.read_frame(&mut input, None);
            tokio::pin!(read);
            assert!(poll!(read.as_mut()).is_pending());
        }
        peer.write_all(b"\0\0h").await.unwrap();
        {
            let read = reader.read_frame(&mut input, None);
            tokio::pin!(read);
            assert!(poll!(read.as_mut()).is_pending());
        }
        peer.write_all(b"elloRAW").await.unwrap();
        assert_eq!(
            reader.read_frame(&mut input, None).await.unwrap(),
            &b"hello"[..]
        );
        assert_eq!(reader.bytes.as_ref(), b"RAW");
    }

    #[tokio::test]
    async fn cancelled_raw_read_does_not_change_destination_or_discard_data() {
        let (mut input, mut peer) = duplex(16);
        let mut reader = ReadBuffer::new(16);
        let mut buf = [0xff; 4];
        peer.write_all(b"ab").await.unwrap();
        {
            let read = reader.read_exact(&mut input, None, &mut buf);
            tokio::pin!(read);
            assert!(poll!(read.as_mut()).is_pending());
        }
        assert_eq!(buf, [0xff; 4]);
        peer.write_all(b"cd").await.unwrap();
        reader.read_exact(&mut input, None, &mut buf).await.unwrap();
        assert_eq!(&buf, b"abcd");
    }

    #[tokio::test]
    async fn invalid_frame_lengths_are_rejected_from_header_alone() {
        let (mut input, mut peer) = duplex(16);
        for len in [0, u32::MAX] {
            let mut reader = ReadBuffer::new(16);
            peer.write_u32_le(len).await.unwrap();
            let read = reader.read_frame(&mut input, None);
            tokio::pin!(read);
            assert!(
                matches!(poll!(read.as_mut()), Poll::Ready(Err(Error::Connection(message)))
                if message == format!("Invalid frame length: {len}"))
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn inactivity_restarts_on_socket_progress() {
        let (mut input, mut peer) = duplex(16);
        let mut reader = ReadBuffer::new(16);
        let inactivity = Inactivity::new(Duration::from_secs(5));
        let read = reader.read_frame(&mut input, Some(&inactivity));
        tokio::pin!(read);
        assert!(poll!(read.as_mut()).is_pending());
        advance(Duration::from_secs(4)).await;
        peer.write_u32_le(2).await.unwrap();
        assert!(poll!(read.as_mut()).is_pending());
        advance(Duration::from_secs(4)).await;
        assert!(poll!(read.as_mut()).is_pending());
        advance(Duration::from_secs(2)).await;
        assert!(matches!(
            poll!(read.as_mut()),
            Poll::Ready(Err(Error::Timeout(_)))
        ));
    }

    #[tokio::test]
    async fn socks5_authenticates_and_connects_without_consuming_tunnel_bytes() {
        let proxy = FakeServer::bind().await;
        let opts = ConnectionOptions {
            proxy: Some(
                ProxyOptions::new(
                    proxy.addr().ip().to_string(),
                    proxy.addr().port(),
                    Some(("user".into(), "pass".into())),
                )
                .unwrap(),
            ),
            ..Default::default()
        };
        let target: SocketAddr = "[2001:db8::1]:4321".parse().unwrap();
        let server = tokio::spawn(async move {
            let mut stream = proxy.accept().await;
            let mut greeting = [0; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            assert_eq!(greeting, [5, 1, 2]);
            stream.write_all(&[5, 2]).await.unwrap();
            let mut credentials = [0; 11];
            stream.read_exact(&mut credentials).await.unwrap();
            assert_eq!(&credentials, b"\x01\x04user\x04pass");
            stream.write_all(&[1, 0]).await.unwrap();
            let mut request = [0; 22];
            stream.read_exact(&mut request).await.unwrap();
            assert_eq!(&request[..4], &[5, 1, 0, 4]);
            assert_eq!(
                &request[4..20],
                &"2001:db8::1"
                    .parse::<std::net::Ipv6Addr>()
                    .unwrap()
                    .octets()
            );
            assert_eq!(&request[20..], &4321u16.to_be_bytes());
            stream
                .write_all(b"\x05\0\0\x03\x03abc\0\0\x02\0\0\0ok")
                .await
                .unwrap();
        });

        let mut conn = Connection::connect(target, &opts).await.unwrap();
        assert_eq!(conn.remote(), target);
        assert_eq!(conn.read_frame().await.unwrap(), &b"ok"[..]);
        server.await.unwrap();
    }
}
