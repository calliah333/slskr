pub(crate) mod connection;
pub(crate) mod listener;
pub(crate) mod message_connection;
pub(crate) mod rate;
pub(crate) mod waiter;

#[cfg(test)]
pub(crate) mod test_support {
    use std::net::{Ipv4Addr, SocketAddr};

    use bytes::{Bytes, BytesMut};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    use super::connection::MAX_FRAME_BODY_LEN;

    pub(crate) struct FakeServer(TcpListener);

    impl FakeServer {
        pub(crate) async fn bind() -> Self {
            Self(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap())
        }

        pub(crate) fn addr(&self) -> SocketAddr {
            self.0.local_addr().unwrap()
        }

        pub(crate) async fn accept(&self) -> TcpStream {
            self.0.accept().await.unwrap().0
        }
    }

    /// Reads a frame body (code and payload), without its length prefix.
    pub(crate) async fn read_frame(stream: &mut TcpStream) -> Bytes {
        let len = stream.read_u32_le().await.unwrap() as usize;
        assert!((1..=MAX_FRAME_BODY_LEN).contains(&len));
        let mut body = BytesMut::zeroed(len);
        stream.read_exact(&mut body).await.unwrap();
        body.freeze()
    }

    /// Writes a frame from its body (code and payload), adding the length prefix.
    pub(crate) async fn write_frame(stream: &mut TcpStream, body: &[u8]) {
        assert!((1..=MAX_FRAME_BODY_LEN).contains(&body.len()));
        stream.write_u32_le(body.len() as u32).await.unwrap();
        stream.write_all(body).await.unwrap();
    }
}
