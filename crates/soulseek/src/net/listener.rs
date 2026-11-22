//! Listener for inbound peer, file-transfer and distributed connections.

use std::future::poll_fn;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, Weak};
use std::task::Poll;

use tokio::net::{TcpListener, TcpSocket};
use tokio_util::sync::CancellationToken;

use crate::client::{ClientInner, SoulseekClient};
use crate::error::{Error, Result};
use crate::options::ConnectionOptions;
use crate::protocol::init::{InitIncoming, PeerInit, PierceFirewall};

use super::connection::Connection;

pub(crate) struct Listener {
    socket: Arc<Mutex<Option<TcpListener>>>,
    stop: CancellationToken,
}

impl Listener {
    pub(crate) fn bind(endpoint: SocketAddr, backlog: u32) -> Result<TcpListener> {
        let bind = || {
            let socket = if endpoint.is_ipv4() {
                TcpSocket::new_v4()?
            } else {
                TcpSocket::new_v6()?
            };
            #[cfg(unix)]
            socket.set_reuseaddr(true)?;
            socket.bind(endpoint)?;
            socket.listen(backlog)
        };
        bind().map_err(|error| {
            Error::Listen(format!("Failed to start listening on {endpoint}: {error}"))
        })
    }

    pub(crate) fn spawn(
        socket: TcpListener,
        options: ConnectionOptions,
        client: Weak<ClientInner>,
    ) -> Self {
        let socket = Arc::new(Mutex::new(Some(socket)));
        let accept_socket = socket.clone();
        let stop = CancellationToken::new();
        let cancel = stop.clone();
        tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    accepted = poll_fn(|cx| {
                        let socket = accept_socket.lock().unwrap_or_else(|e| e.into_inner());
                        match socket.as_ref() {
                            Some(socket) => socket.poll_accept(cx).map(Some),
                            None => Poll::Ready(None),
                        }
                    }) => accepted,
                };
                let Some(accepted) = accepted else { break };
                match accepted {
                    Ok((stream, remote)) => {
                        let conn = Connection::from_accepted(stream, remote, &options);
                        let client = client.clone();
                        let cancel = cancel.clone();
                        tokio::spawn(async move {
                            tokio::select! {
                                biased;
                                _ = cancel.cancelled() => {},
                                _ = handle_init(client, &cancel, conn) => {},
                            }
                        });
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Soulseek peer listener failed to accept a connection");
                        // Avoid spinning on persistent accept failures, e.g. descriptor exhaustion.
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {},
                        }
                    }
                }
            }
        });
        Self { socket, stop }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        // Release the port synchronously so an immediate reconnect can bind it.
        self.socket.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.stop.cancel();
    }
}

async fn handle_init(client: Weak<ClientInner>, stop: &CancellationToken, mut conn: Connection) {
    let init = match conn
        .read_frame()
        .await
        .and_then(|body| Ok(InitIncoming::decode(body)?))
    {
        Ok(init) => init,
        Err(error) => {
            tracing::debug!(remote = %conn.remote(), %error, "Invalid Soulseek peer initialization");
            return;
        }
    };
    let Some(inner) = client.upgrade() else {
        return;
    };
    let client = SoulseekClient::from_inner(inner);
    client
        .inner()
        .server
        .with_live_listener(client.inner(), stop, || match init {
            InitIncoming::PierceFirewall(PierceFirewall { token }) => {
                client
                    .inner()
                    .peers
                    .handle_pierce_firewall(&client, token, conn);
            }
            InitIncoming::PeerInit(PeerInit {
                username,
                connection_type,
                token,
            }) => match connection_type.as_str() {
                "P" => client
                    .inner()
                    .peers
                    .handle_incoming_peer_init(&client, username, token, conn),
                "F" => client
                    .inner()
                    .transfers
                    .handle_incoming_transfer_connection(&client, username, conn),
                "D" => client
                    .inner()
                    .distributed
                    .handle_incoming_child(&client, username, conn),
                _ => tracing::debug!(%connection_type, "Unknown Soulseek peer connection type"),
            },
            InitIncoming::Unknown { code, .. } => {
                tracing::debug!(code, "Unknown Soulseek peer initialization code");
            }
        });
}
