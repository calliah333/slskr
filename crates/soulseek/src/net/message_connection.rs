use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use tokio::sync::{Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::connection::{Connection, FrameObserver, frame_body_len, until_idle, write_all};
use crate::error::{Error, Result};

const INCOMING_CAPACITY: usize = 16;
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

type Write = (Bytes, oneshot::Sender<Result<()>>);

#[derive(Clone)]
pub(crate) struct MessageConnection(Arc<Inner>);

struct Inner {
    outgoing: mpsc::Sender<Write>,
    closed: CancellationToken,
    remote: SocketAddr,
    id: u64,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}

impl MessageConnection {
    /// Starts the reader and writer. The receiver yields frame bodies without the length prefix.
    pub fn spawn(conn: Connection) -> (Self, mpsc::Receiver<Bytes>) {
        Self::spawn_observed(conn, None)
    }

    /// Like [`Self::spawn`]; `observer` sees the progress of each frame while it is read.
    pub fn spawn_observed(
        conn: Connection,
        observer: Option<FrameObserver>,
    ) -> (Self, mpsc::Receiver<Bytes>) {
        let remote = conn.remote();
        let capacity = conn.write_queue_size().clamp(1, Semaphore::MAX_PERMITS);
        let (mut read, mut write, mut buffer, inactivity) = conn.into_framed_parts();
        let (outgoing, mut writes) = mpsc::channel::<Write>(capacity);
        let (incoming, frames) = mpsc::channel(INCOMING_CAPACITY);
        let closed = CancellationToken::new();

        let read_closed = closed.clone();
        let read_inactivity = inactivity.clone();
        let read_guard = closed.clone().drop_guard();
        tokio::spawn(async move {
            let _guard = read_guard;
            let inactivity = read_inactivity.as_deref();
            loop {
                let frame = tokio::select! {
                    biased;
                    () = read_closed.cancelled() => break,
                    () = incoming.closed() => break,
                    result = buffer.read_frame_observed(&mut read, inactivity, observer.as_ref()) => match result {
                        Ok(frame) => frame,
                        Err(err) => {
                            tracing::debug!(%remote, %err, "Message connection read stopped");
                            break;
                        }
                    },
                };
                tokio::select! {
                    biased;
                    () = read_closed.cancelled() => break,
                    result = until_idle(inactivity, incoming.send(frame)) => match result {
                        Ok(Ok(())) => {},
                        _ => break,
                    },
                }
            }
        });

        let write_closed = closed.clone();
        let write_guard = closed.clone().drop_guard();
        tokio::spawn(async move {
            let _guard = write_guard;
            loop {
                let (frame, done) = tokio::select! {
                    biased;
                    () = write_closed.cancelled() => break,
                    item = writes.recv() => match item {
                        Some(item) => item,
                        None => break,
                    },
                };
                let result = tokio::select! {
                    biased;
                    () = write_closed.cancelled() => break,
                    result = write_all(&mut write, inactivity.as_deref(), &frame) => result,
                };
                let failed = result.is_err();
                let _ = done.send(result);
                if failed {
                    break;
                }
            }
        });

        let handle = Self(Arc::new(Inner {
            outgoing,
            closed,
            remote,
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        }));
        (handle, frames)
    }

    /// Writes a complete length-prefixed frame. Once queued, dropping this future does not
    /// interrupt the frame; the writer either finishes it or closes the socket.
    pub async fn send(&self, frame: Bytes) -> Result<()> {
        let remote = self.remote();
        if self.is_closed() {
            return Err(closed_error(remote));
        }
        let header = frame
            .get(..4)
            .ok_or_else(|| Error::Connection("Missing frame length".into()))?;
        let len = frame_body_len(header.try_into().unwrap())?;
        if frame.len() - header.len() != len {
            return Err(Error::Connection(
                "Frame length does not match its body".into(),
            ));
        }

        let permit = match self.0.outgoing.try_reserve() {
            Ok(permit) => permit,
            Err(mpsc::error::TrySendError::Closed(())) => return Err(closed_error(remote)),
            Err(mpsc::error::TrySendError::Full(())) => {
                self.close();
                return Err(Error::Connection(format!(
                    "Write queue to {remote} is full"
                )));
            }
        };
        let (done, written) = oneshot::channel();
        permit.send((frame, done));
        written.await.unwrap_or_else(|_| Err(closed_error(remote)))
    }

    pub fn close(&self) {
        self.0.closed.cancel();
    }

    pub fn is_closed(&self) -> bool {
        self.0.closed.is_cancelled()
    }

    pub async fn closed(&self) {
        self.0.closed.cancelled().await;
    }

    pub fn remote(&self) -> SocketAddr {
        self.0.remote
    }

    pub fn id(&self) -> u64 {
        self.0.id
    }
}

fn closed_error(remote: SocketAddr) -> Error {
    Error::Connection(format!("Message connection to {remote} is closed"))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::poll;
    use tokio::io::AsyncReadExt;
    use tokio::time::timeout;

    use super::*;
    use crate::net::test_support::{FakeServer, read_frame, write_frame};
    use crate::options::ConnectionOptions;
    use crate::protocol::MessageBuilder;

    #[tokio::test]
    async fn exchanges_frames_and_closes_both_halves_on_peer_eof() {
        let server = FakeServer::bind().await;
        let conn = Connection::connect(server.addr(), &ConnectionOptions::default())
            .await
            .unwrap();
        let mut peer = server.accept().await;
        let (conn, mut incoming) = MessageConnection::spawn(conn);

        write_frame(&mut peer, b"\x07reply").await;
        assert_eq!(incoming.recv().await.unwrap(), &b"\x07reply"[..]);

        let mut builder = MessageBuilder::new();
        builder.code_u32(23).bytes(b"request");
        conn.send(builder.build()).await.unwrap();
        assert_eq!(read_frame(&mut peer).await, &b"\x17\0\0\0request"[..]);
        drop(peer);

        conn.closed().await;
        assert_eq!(incoming.recv().await, None);
        assert!(matches!(
            conn.send(builder.build()).await,
            Err(Error::Connection(_))
        ));
    }

    #[tokio::test]
    async fn queued_frame_survives_cancellation_of_send_future() {
        let server = FakeServer::bind().await;
        let conn = Connection::connect(server.addr(), &ConnectionOptions::default())
            .await
            .unwrap();
        let mut peer = server.accept().await;
        let (conn, _incoming) = MessageConnection::spawn(conn);
        {
            let write = conn.send(Bytes::from_static(b"\x02\0\0\0ab"));
            tokio::pin!(write);
            assert!(poll!(write.as_mut()).is_pending());
        }
        assert_eq!(read_frame(&mut peer).await, &b"ab"[..]);
        conn.close();
        let mut byte = [0; 1];
        assert_eq!(
            timeout(Duration::from_secs(1), peer.read(&mut byte))
                .await
                .unwrap()
                .unwrap(),
            0
        );
    }
}
